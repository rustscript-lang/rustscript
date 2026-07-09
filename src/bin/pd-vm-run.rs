fn main() -> Result<(), Box<dyn std::error::Error>> {
    vm::cli::main(vm::cli::CliRuntime::default())
}
