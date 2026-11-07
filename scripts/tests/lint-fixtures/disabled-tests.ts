// ruleid: javascript-disabled-test-api
test.skip("later", () => {});
// ruleid: javascript-disabled-test-api
test.concurrent.skip("later", () => {});
// ruleid: javascript-disabled-test-api
test.skipIf(true)("later", () => {});
// ruleid: javascript-disabled-test-api
test.only("later", () => {});
// ruleid: javascript-disabled-test-api
describe["skip"]("later", () => {});
// ruleid: javascript-focused-test-alias
xit("later", () => {});
// ruleid: javascript-disabled-test-option
test("later", { skip: true }, () => {});
// ruleid: javascript-disabled-test-option
test("later", { "skip": true }, () => {});
// ruleid: javascript-disabled-test-option
test({ todo: true }, () => {});
// ruleid: javascript-disabled-test-option
context.test({ only: true }, () => {});
// ruleid: javascript-disabled-test-api
test("later", context => context.skip("later"));
// ruleid: javascript-disabled-test-api
test("later", context => context.todo("later"));
// ruleid: javascript-disabled-test-api
test("later", () => { const fixture = `${context.skip("later")}`; });
const quote = () => /"/;
// ruleid: javascript-disabled-test-api
test.skip("later", () => {});
const expression = enabled && /"/;
// ruleid: javascript-disabled-test-api
test.skip("later", () => {});

// ok: javascript-disabled-test-api, javascript-focused-test-alias, javascript-disabled-test-option
const fixture: string = 'test.skip("later", () => {})';
// ok: javascript-disabled-test-api
const template = `test.skip("later", () => {})`;
// ok: javascript-disabled-test-api
const regex = /test\.skip\("later"\)/;
// ok: javascript-disabled-test-option
test("pagination", () => { assert.deepEqual(result, { skip: 0 }); });
// ok: javascript-disabled-test-option
test("works", { timeout: 100 }, () => {});
