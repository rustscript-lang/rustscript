// Actual wasm32 ABI exercise for logical Option/Unit.
//
// Build and run from the repository root:
//   cargo build --locked -p pd-vm-wasm --target wasm32-unknown-unknown --features runtime
//   node pd-vm-wasm/tests/fixtures/option_abi.cjs <path-to-pd_vm_wasm.wasm>
//
// The script instantiates the real exported WebAssembly module through Node and
// asserts the run, language-service hint and format states. It is a manual ABI
// gate, not a native Rust shim and not a build-only assertion.
const fs = require('fs');
const assert = require('assert');

const wasmPath =
  process.argv[2] ||
  'target/wasm32-unknown-unknown/debug/pd_vm_wasm.wasm';
const wasmModule = new WebAssembly.Module(fs.readFileSync(wasmPath));
const instance = new WebAssembly.Instance(wasmModule, {
  env: { pd_playground_now_ms: () => BigInt(Date.now()) },
});
const wasmExports = instance.exports;
const memory = wasmExports.memory;

function put(text) {
  const bytes = Buffer.from(text, 'utf8');
  const ptr = wasmExports.wasm_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

function read(packed) {
  const len = Number(packed >> 32n);
  const ptr = Number(packed & 0xffffffffn);
  const bytes = new Uint8Array(memory.buffer, ptr, len);
  const text = Buffer.from(bytes).toString('utf8');
  wasmExports.wasm_dealloc(ptr, len);
  return text;
}

function runSource(source) {
  const [sptr, slen] = put(source);
  const [fptr, flen] = put('rss');
  const [optr, olen] = put('{"fuel":100000,"fuelCheckInterval":1}');
  return JSON.parse(read(wasmExports.run_source_json(sptr, slen, fptr, flen, optr, olen)));
}

function hints(source) {
  const [sptr, slen] = put(source);
  const [fptr, flen] = put('rss');
  return JSON.parse(read(wasmExports.local_type_hints_json(sptr, slen, fptr, flen)));
}

function format(source) {
  const [sptr, slen] = put(source);
  const [fptr, flen] = put('rss');
  return JSON.parse(read(wasmExports.format_source_json(sptr, slen, fptr, flen)));
}

const runtimeSource = `
fn relay(value: Option<int>) -> Option<int> { match value { Option::Some(v) => Option::Some(v + 1), Option::None => Option::None } }
let some = relay(Option::Some(7));
let none = relay(Option::None);
match some { Option::Some(value) => print("some:" + value), Option::None => print("some:none"), };
match none { Option::Some(value) => print("none:some"), Option::None => print("none:none"), };
let unit: () = ();
print("unit");
`;

const runtime = runSource(runtimeSource);
console.log('WASM_OPTION_RUNTIME', JSON.stringify(runtime));
assert.strictEqual(runtime.ok, true, JSON.stringify(runtime));
assert.strictEqual(runtime.error, null);
assert.strictEqual(runtime.halted, true);
assert.deepStrictEqual(runtime.output, ['some:8', 'none:none', 'unit']);

const hintSource = `
let nested: Option<Option<int>> = Option::Some(Option::None);
let unit: () = ();
let empty: Option<string> = Option::Some("");
let flag: Option<bool> = Option::Some(false);
let zero: Option<int> = Option::Some(0);
`;
const hintPayload = hints(hintSource);
console.log('WASM_OPTION_HINTS', JSON.stringify(hintPayload));
assert.ok(!hintPayload.error, JSON.stringify(hintPayload));
const byName = new Map(hintPayload.hints.map((hint) => [hint.name, hint]));
assert.deepStrictEqual(byName.get('nested').schema, {
  kind: 'option',
  inner: { kind: 'option', inner: { kind: 'int' } },
});
assert.deepStrictEqual(byName.get('unit').schema, { kind: 'unit' });
assert.deepStrictEqual(byName.get('empty').schema, {
  kind: 'option',
  inner: { kind: 'string' },
});
assert.deepStrictEqual(byName.get('flag').schema, {
  kind: 'option',
  inner: { kind: 'bool' },
});
assert.deepStrictEqual(byName.get('zero').schema, {
  kind: 'option',
  inner: { kind: 'int' },
});

const formatSourceText =
  'fn relay(value:Option<int>)->Option<int>{match value{Option::Some(v)=>Option::Some(v+1),Option::None=>Option::None}}\n' +
  'let value:Option<()> =Option::Some(());\nlet result=relay(Option::Some(1));\n';
const first = format(formatSourceText);
console.log('WASM_OPTION_FORMAT', JSON.stringify(first));
assert.strictEqual(first.ok, true, JSON.stringify(first));
assert.ok(first.formatted.includes('Option<int>'), first.formatted);
assert.ok(first.formatted.includes('Option<()>'), first.formatted);
const second = format(first.formatted);
assert.deepStrictEqual(second, first);
const formattedRun = runSource(first.formatted);
assert.strictEqual(formattedRun.ok, true, JSON.stringify(formattedRun));

console.log('WASM_OPTION_ABI_OK');
