//! HTTPS-only JavaScript modules. Cache source bytes, never evaluated instances.
use deno_core::{
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleSource,
    ModuleSourceCode, ModuleSpecifier, ModuleType, RequestedModuleType, ResolutionKind,
    error::ModuleLoaderError, resolve_import,
};
use deno_runtime::deno_permissions::PermissionsContainer;
use std::{
    cell::Cell,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_MODULE: usize = 2 * 1024 * 1024;
const MAX_GRAPH: usize = 8 * 1024 * 1024;
const MAX_MODULES: usize = 64;
const MAX_CACHE: usize = 32 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 128;
const CACHE_TTL: Duration = Duration::from_secs(3600);

type Error = ModuleLoaderError;

fn download_error(error: reqwest::Error) -> Error {
    let mut cause: &dyn std::error::Error = &error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    Error::generic(format!(
        "Module download failed: {cause} ({})",
        error.url().map_or("unknown URL", |url| url.as_str())
    ))
}

#[derive(Clone)]
struct CachedModule {
    source: Arc<str>,
    // Keep every redirect for permission checks even on a cache hit.
    urls: Vec<ModuleSpecifier>,
    fetched_at: Instant,
}

#[derive(Clone, Default)]
pub(super) struct ModuleCache(Arc<Mutex<VecDeque<(String, CachedModule)>>>);

impl ModuleCache {
    fn get(&self, url: &str) -> Option<CachedModule> {
        let mut cache = self.0.lock().unwrap();
        cache.retain(|(_, entry)| entry.fetched_at.elapsed() < CACHE_TTL);
        cache
            .iter()
            .find(|(key, _)| key == url)
            .map(|(_, entry)| entry.clone())
    }

    fn insert(&self, url: String, entry: CachedModule) {
        let mut cache = self.0.lock().unwrap();
        cache.retain(|(key, entry)| key != &url && entry.fetched_at.elapsed() < CACHE_TTL);
        let mut bytes = cache
            .iter()
            .map(|(_, entry)| entry.source.len())
            .sum::<usize>();
        while cache.len() >= MAX_CACHE_ENTRIES || bytes + entry.source.len() > MAX_CACHE {
            let Some((_, old)) = cache.pop_front() else {
                break;
            };
            bytes -= old.source.len();
        }
        cache.push_back((url, entry));
    }
}

#[derive(Default)]
struct Budget {
    modules: Cell<usize>,
    bytes: Cell<usize>,
}

#[derive(Clone)]
pub(super) struct HttpsModuleLoader {
    client: reqwest::Client,
    permissions: PermissionsContainer,
    public_only: bool,
    cache: ModuleCache,
    budget: Rc<Budget>,
}

impl HttpsModuleLoader {
    pub(super) fn new(
        permissions: PermissionsContainer,
        public_only: bool,
        cache: ModuleCache,
    ) -> Result<Self, Error> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // A proxy could bypass the checked DNS resolver.
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .user_agent("Pastebin-ESM/1");
        if public_only {
            builder = builder.dns_resolver(Arc::new(super::PublicResolver));
        }
        Ok(Self {
            client: builder.build().map_err(|e| Error::generic(e.to_string()))?,
            permissions,
            public_only,
            cache,
            budget: Rc::new(Budget::default()),
        })
    }

    fn check_url(&self, url: &ModuleSpecifier) -> Result<(), Error> {
        let allowed_scheme = url.scheme() == "https";
        // Local fixtures only; HTTP imports cannot be enabled in production.
        #[cfg(test)]
        let allowed_scheme = allowed_scheme || (!self.public_only && url.scheme() == "http");
        if !allowed_scheme {
            return Err(Error::type_error(
                "Module imports require an HTTPS URL serving JavaScript ESM; npm:, jsr:, file:, data: and bare package names are not supported",
            ));
        }
        if url.as_str().len() > 4096 || !url.username().is_empty() || url.password().is_some() {
            return Err(Error::type_error(
                "Module URL must be at most 4096 characters and contain no credentials",
            ));
        }
        if self.public_only {
            if let Some(host) = url.host_str() {
                if let Ok(ip) = host.trim_matches(['[', ']']).parse() {
                    if !super::public_ip(ip) {
                        return Err(Error::type_error(
                            "Private and reserved module addresses are not allowed",
                        ));
                    }
                }
            }
        }
        self.permissions
            .clone()
            .check_net_url(url, "import()")
            .map_err(Error::from_err)
    }

    async fn download(&self, url: &ModuleSpecifier) -> Result<CachedModule, Error> {
        let mut current = url.clone();
        let mut urls = Vec::new();
        for redirect in 0..=5 {
            self.check_url(&current)?;
            urls.push(current.clone());
            let mut response = self
                .client
                .get(current.clone())
                .send()
                .await
                .map_err(download_error)?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                if redirect == 5 {
                    return Err(Error::type_error("Module exceeded 5 redirects"));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|header| header.to_str().ok())
                    .ok_or_else(|| Error::type_error("Module redirect has no valid Location"))?;
                current = current.join(location).map_err(Error::from_err)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(Error::generic(format!(
                    "Module download failed: HTTP {} ({current})",
                    response.status()
                )));
            }
            let mime = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !matches!(
                mime.as_str(),
                "text/javascript"
                    | "application/javascript"
                    | "application/ecmascript"
                    | "text/ecmascript"
                    | "application/x-javascript"
            ) {
                return Err(Error::type_error(format!(
                    "Expected JavaScript ESM, received Content-Type {mime:?} ({current})"
                )));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(download_error)? {
                if body.len() + chunk.len() > MAX_MODULE {
                    return Err(Error::range_error("Module source exceeds 2 MiB"));
                }
                let bytes = self.budget.bytes.get() + chunk.len();
                if bytes > MAX_GRAPH {
                    return Err(Error::range_error(
                        "Module sources exceed 8 MiB per execution",
                    ));
                }
                self.budget.bytes.set(bytes);
                body.extend_from_slice(&chunk);
            }
            let source = String::from_utf8(body)
                .map_err(|_| Error::type_error("Module source must be UTF-8"))?;
            return Ok(CachedModule {
                source: source.into(),
                urls,
                fetched_at: Instant::now(),
            });
        }
        unreachable!()
    }

    async fn source(&self, url: ModuleSpecifier) -> Result<ModuleSource, Error> {
        self.check_url(&url)?;
        let entry = if let Some(entry) = self.cache.get(url.as_str()) {
            for hop in &entry.urls {
                self.check_url(hop)?;
            }
            let bytes = self.budget.bytes.get() + entry.source.len();
            if bytes > MAX_GRAPH {
                return Err(Error::range_error(
                    "Module sources exceed 8 MiB per execution",
                ));
            }
            self.budget.bytes.set(bytes);
            entry
        } else {
            let entry = tokio::time::timeout(Duration::from_secs(10), self.download(&url))
                .await
                .map_err(|_| Error::generic("Module download timed out after 10 seconds"))??;
            self.cache.insert(url.to_string(), entry.clone());
            entry
        };
        Ok(ModuleSource::new_with_redirect(
            ModuleType::JavaScript,
            ModuleSourceCode::String(entry.source.to_string().into()),
            &url,
            entry.urls.last().unwrap(),
            None,
        ))
    }
}

impl ModuleLoader for HttpsModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, Error> {
        let url = resolve_import(specifier, referrer).map_err(Error::from_err)?;
        // The entry is supplied from the database, never read from disk.
        if kind == ResolutionKind::MainModule && url.as_str() == "file:///paste.js" {
            return Ok(url);
        }
        self.check_url(&url)?;
        Ok(url)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        if options.requested_module_type != RequestedModuleType::None {
            return ModuleLoadResponse::Sync(Err(Error::type_error(
                "Only JavaScript ESM imports are supported",
            )));
        }
        let count = self.budget.modules.get() + 1;
        if count > MAX_MODULES {
            return ModuleLoadResponse::Sync(Err(Error::range_error(
                "Module imports exceed 64 per execution",
            )));
        }
        self.budget.modules.set(count);
        let loader = self.clone();
        let url = specifier.clone();
        ModuleLoadResponse::Async(Box::pin(async move { loader.source(url).await }))
    }
}

impl reqwest::dns::Resolve for super::PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().to_owned();
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addresses.is_empty() || addresses.iter().any(|addr| !super::public_ip(addr.ip())) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Private and reserved module addresses are not allowed",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}
