// ruleid: javascript-disabled-test-api
test.skip("later", () => {});
// ruleid: javascript-disabled-test-option
test("later", { "todo": true }, () => {});
// ruleid: javascript-focused-test-alias
xtest("later", () => {});
// ok: javascript-disabled-test-api
const fixture = 'test.skip("later", () => {})';
// ok: javascript-disabled-test-option
test("pagination", () => { assert.deepEqual(result, { skip: 0 }); });
