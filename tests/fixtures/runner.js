export default async function () {
  console.log("runner log");
  await Promise.resolve();
  return "runner result";
}
