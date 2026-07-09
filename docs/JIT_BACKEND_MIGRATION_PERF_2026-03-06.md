# JIT Backend Migration Perf Record (2026-03-06)

This record captures the handwritten-vs-cranelift comparison run before removing the handwritten native backend.

Environment:

- Host: Windows x86_64 (local dev machine)
- Command profile: `--release`
- Crate: `pd-vm`
- Feature set: `--features cranelift-jit`

Commands:

```powershell
$env:PD_VM_JIT_CODEGEN='handwritten'
cargo test -p pd-vm --release --features cranelift-jit --test jit_tests perf_jit_native_reduces_tight_loop_latency -- --ignored --nocapture

$env:PD_VM_JIT_CODEGEN='cranelift'
cargo test -p pd-vm --release --features cranelift-jit --test jit_tests perf_jit_native_reduces_tight_loop_latency -- --ignored --nocapture

$env:PD_VM_JIT_CODEGEN='handwritten'
$env:PDVM_PERF_AES_JIT_DIAG='1'
cargo test -p pd-vm --release --features cranelift-jit --test jit_tests perf_manual_aes_128_cbc_rustscript_matches_in_interpreter_jit_and_aot -- --ignored --nocapture

$env:PD_VM_JIT_CODEGEN='cranelift'
$env:PDVM_PERF_AES_JIT_DIAG='1'
cargo test -p pd-vm --release --features cranelift-jit --test jit_tests perf_manual_aes_128_cbc_rustscript_matches_in_interpreter_jit_and_aot -- --ignored --nocapture
```

Results:

| Test | Backend | Interpreter | JIT | AOT run | AOT prepare |
|---|---|---:|---:|---:|---:|
| `perf_jit_native_reduces_tight_loop_latency` | handwritten | 124 ms | 19 ms | 19 ms | n/a |
| `perf_jit_native_reduces_tight_loop_latency` | cranelift | 125 ms | 18 ms | 18 ms | n/a |
| `perf_manual_aes_128_cbc_rustscript_matches_in_interpreter_jit_and_aot` | handwritten | 28861 us | 27367 us | 25523 us | 2408 us |
| `perf_manual_aes_128_cbc_rustscript_matches_in_interpreter_jit_and_aot` | cranelift | 28518 us | 22433 us | 20088 us | 2024 us |

Conclusion:

- No runtime perf gap was found that blocks migration.
- In these release runs, Cranelift was at least parity on tight-loop latency and faster on the AES workload.
