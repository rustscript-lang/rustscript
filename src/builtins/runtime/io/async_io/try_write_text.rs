use pd_host_function::pd_host_function;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use super::super::{IoWriteTextResult, io_write_text_precheck};
use super::{IoPolicyContext, authorize_io_path};
use crate::vm::{HostFutureOutput, Value, VmResult};

/// Writes a whole UTF-8 text file and returns ordinary policy or OS failures as data.
#[pd_host_function(name = "io::try_write_text", contract = super::super::io_try_write_text_contract)]
pub(crate) async fn builtin_io_try_write_text(
    #[pd_host_context] context: IoPolicyContext,
    path: String,
    text: String,
) -> VmResult<HostFutureOutput<Value>> {
    let result = match authorize_io_path(context.policy.as_ref(), &path, true).await {
        Ok(path) => match io_write_text_precheck(context.policy.as_ref(), &text) {
            Some(result) => result,
            None => {
                let write = async {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(path)
                        .await?;
                    file.write_all(text.as_bytes()).await?;
                    file.flush().await?;
                    file.shutdown().await?;
                    drop(file);
                    Ok::<(), std::io::Error>(())
                }
                .await;
                match write {
                    Ok(()) => IoWriteTextResult::success(),
                    Err(_) => IoWriteTextResult::failure("io_error"),
                }
            }
        },
        Err(error) => IoWriteTextResult::failure(error.result_code()),
    };
    Ok(HostFutureOutput::returning(result.into_value()))
}
