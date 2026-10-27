/// The command runs on its own thread with this stack. An unoptimised build
/// gives each awaited future of a command its own stack frame copies, and a
/// debug `validate` or `sync` passed the main thread's 8 MiB;
/// the pages are only reserved, so a release build uses no more memory.
const COMMAND_STACK_BYTES: usize = 64 * 1024 * 1024;

fn main() -> anyhow::Result<std::process::ExitCode> {
    std::thread::Builder::new()
        .name("rustunes".into())
        .stack_size(COMMAND_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(rustunes::cli::run())
        })?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}
