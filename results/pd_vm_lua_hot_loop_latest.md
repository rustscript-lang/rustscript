# pd-vm Lua Hot Loop Benchmark

Methodology: compile outside timer, one warmup run outside timer, timed runs only over steady-state execution.

| mode | status | total_us | ns_per_inner_iter |
| --- | --- | ---: | ---: |
| pd-vm | ok | 123 | 3097.50 |
| pd-vm-jit | ok | 20 | 502.50 |
| luajit-joff | luajit not found in PATH | - | - |
| luajit-jit | luajit not found in PATH | - | - |
