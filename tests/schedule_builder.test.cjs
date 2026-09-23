const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const template = fs.readFileSync(new URL('../templates/schedule_builder_script.html', `file://${__filename}`), 'utf8');
const context = {window:{}};
vm.runInNewContext(template.match(/<script>([\s\S]*?)<\/script>/)[1], context);
const builder = context.window.ScheduleBuilder;

test('existing supported schedules retain their execution times when edited', () => {
  for (const cron of ['* * * * *','*/7 * * * *','*/30 * * * *','0 * * * *','15 */4 * * *','45 23 * * *','30 8 * * 1-5','0 10 * * 0,6','0 0 31 * *']) {
    assert.equal(builder.build(builder.parse(cron)),cron);
  }
});
test('complex expressions stay in Cron mode instead of losing constraints', () => {
  for (const cron of ['0 9 1 * 1','0 9 * 1 *','0,30 * * * *','0 9 * * MON','@daily']) {
    assert.equal(builder.parse(cron).type,'custom');
  }
});
test('builder rejects missing, fractional and out-of-range inputs', () => {
  for (const interval of ['',0,-1,1.5,60]) assert.equal(builder.build({type:'minutes',interval}),null);
  assert.equal(builder.build({type:'hours',interval:24,minute:0}),null);
  assert.equal(builder.build({type:'hours',interval:2,minute:''}),null);
  assert.equal(builder.build({type:'daily',time:'24:00'}),null);
  assert.equal(builder.build({type:'monthly',time:'09:00',monthday:32}),null);
});
test('calendar controls generate the expected five-field Cron', () => {
  assert.equal(builder.build({type:'hours',interval:6,minute:30}),'30 */6 * * *');
  assert.equal(builder.build({type:'weekly',weekday:'1-5',time:'08:45'}),'45 8 * * 1-5');
  assert.equal(builder.build({type:'monthly',monthday:15,time:'12:30'}),'30 12 15 * *');
});
