//! Launching and communication with the subprocess

use super::{
    de::{ChildResponse, RefinedChildResponse, Status, Timings},
    ser::ChildCommand,
    CallFailure, Command, SharedReceiver, SubProcessEvent, SubprocessError, SubprocessExitReason,
};
use anyhow::Context;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, trace, warn, Instrument};

/// Launches a python subprocess, and executes calls on it until shutdown is initiated.
///
/// If the python process is killed, reaping it and restarting new one is handled by [`super::start`],
/// similarly to spawning this as a task usually handled.
///
/// Launching happens in two stages, similar to the python process. Initially we only launch, then
/// read `"ready\n"` from the subprocess and after that enter the loop where we contend for the
/// commands.
#[tracing::instrument(name = "subproc", skip_all, fields(pid))]
pub(super) async fn launch_python(
    database_path: PathBuf,
    commands: SharedReceiver<(Command, tracing::Span)>,
    status_updates: mpsc::Sender<SubProcessEvent>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> anyhow::Result<(u32, Option<std::process::ExitStatus>, SubprocessExitReason)> {
    let (mut child, pid, mut stdin, mut stdout, mut buffer) = match spawn(database_path).await {
        Ok(tuple) => tuple,
        Err(e) => {
            return Err(e.context("Failed to start python subprocess"));
        }
    };

    if status_updates
        .send(SubProcessEvent::ProcessLaunched(pid))
        .await
        .is_err()
    {
        drop(stdin);
        return Err(anyhow::anyhow!("Failed to notify of start"));
    }

    info!("Subprocess launched");

    let mut command_buffer = Vec::new();

    // TODO: Why not have an outer loop to respawn a process fast? The idea occured during review.
    // Currently the "policy" over respawning is controlled by the "service" in `super::start`.
    let exit_reason = loop {
        let command = async {
            let mut locked = commands.lock().await;
            locked.recv().await
        };

        tokio::pin!(command);

        let (command, span) = tokio::select! {
            // locking is not cancellation safe BUT if the race is lost we don't retry so no
            // worries on that.
            maybe_command = &mut command => match maybe_command {
                Some(tuple) => tuple,
                None => break SubprocessExitReason::Shutdown,
            },
            _ = child.wait() => {
                // if the python process was killed while we were awaiting for new commands, it
                // would be zombie until we notice it has died. The wait can be called many times,
                // and it'll return immediatedly at the top level.
                break SubprocessExitReason::Death;
            }
            _ = shutdown_rx.recv() => {
                break SubprocessExitReason::Shutdown;
            },
        };

        if command.2.is_closed() {
            // quickly loadshed, as the caller has already left.
            continue;
        }

        span.record("pid", &pid);

        let (timings, status) = {
            let op = process(
                command,
                &mut command_buffer,
                &mut stdin,
                &mut stdout,
                &mut buffer,
            )
            .instrument(span);

            tokio::pin!(op);

            tokio::select! {
                res = &mut op => {
                    match res {
                        Ok(t) => t,
                        Err(None) => continue,
                        Err(Some(e)) => break e,
                    }
                },
                _ = shutdown_rx.recv() => {
                    break SubprocessExitReason::Shutdown;
                },
            }
        };

        let send_res = status_updates
            .send(SubProcessEvent::CommandHandled(pid, timings, status))
            .await;

        if send_res.is_err() {
            break SubprocessExitReason::ClosedChannel;
        }

        if !stdout.buffer().is_empty() {
            // some garbage was left in, it shouldn't have; there are extra printlns and we must
            // assume we've gone out of sync now.
            // FIXME: log this, hasn't happened.
            break SubprocessExitReason::UnrecoverableIO;
        }
    };

    trace!(?exit_reason, "Starting to exit");

    // important to close up the stdin not to deadlock
    drop(stdin);
    drop(stdout);

    // give the subprocess a bit of time, since it might be less risky/better for sqlite to
    // exit/cleanup properly
    let sleep = tokio::time::sleep(std::time::Duration::from_millis(1000));
    tokio::pin!(sleep);

    let exit_status = tokio::select! {
        _ = &mut sleep => {
            match child.kill().await {
                Ok(()) => {}
                Err(error) => warn!(%error, "Killing python subprocess failed"),
            }

            // kill already await the child, so there's not much to await here, we should just get the
            // fused response.
            match child.wait().await {
                Ok(status) => Some(status),
                Err(error) => {
                    warn!(%error, "Wait on child pid failed");
                    None
                }
            }
        }
        exit_status = child.wait() => {
            exit_status.ok()
        }
    };

    Ok((pid, exit_status, exit_reason))
}

async fn spawn(
    database_path: PathBuf,
) -> anyhow::Result<(Child, u32, ChildStdin, BufReader<ChildStdout>, String)> {
    // there is not intentionally any std::fs::exists calls to avoid bringing any TOCTOU issues.
    let virtual_env = std::env::var_os("VIRTUAL_ENV").context("VIRTUAL_ENV is not defined")?;

    // FIXME: use choom, add something over /proc/self/oom_score_adj ?
    let mut python_exe = PathBuf::from(&virtual_env);
    python_exe.push("bin");
    python_exe.push("python");

    // we assume that VIRTUAL_ENV is at the base of the `py/` directory
    let mut call_py = PathBuf::from(&virtual_env);
    call_py.push("..");
    call_py.push("src");
    call_py.push("call.py");

    let mut command = tokio::process::Command::new(python_exe);

    command
        .arg(call_py)
        .arg(database_path)
        .env("VIRTUAL_ENV", virtual_env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .context("Failed to spawn the new python process; this should only happen when the session is at it's process limit on unix.")?;

    // why care about the pid? it could be logged, and used to identify process activity, thought
    // these should be easy to spot otherwise as well.
    let pid = child.id().expect("The child pid should had been available after a successful start before waiting for it's status");

    {
        let span = tracing::Span::current();
        span.record("pid", &pid);
    }

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");

    // spawn the stderr out, just forget it it will die down once the process has been torn down
    let _forget = tokio::task::spawn({
        let stderr = child.stderr.take().expect("stderr was piped");
        let span = tracing::trace_span!("stderr");
        async move {
            let mut buffer = String::new();
            let mut reader = BufReader::new(stderr);

            loop {
                buffer.clear();
                match reader.read_line(&mut buffer).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        debug!(error=%e, "stderr read failed, stopping reading");
                        break;
                    }
                }

                trace!("{:?}", buffer.trim());
            }
        }
        .instrument(span)
    });

    // default buffer is fine for us ... but this is double buffering, for no good reason
    // it could actually be destroyed even between runs, because the buffer should be empty
    let mut stdout = BufReader::new(stdout);
    let mut buffer = String::new();

    // reasons for this part to error out:
    // - invalid schema version
    // - some other pythonic thing happens, for example, no call.py found
    let _read = stdout
        .read_line(&mut buffer)
        .await
        .context("Failed to read 'ready' from python process")?;

    anyhow::ensure!(
        // buffer will contain the newline, which doesn't bother serde_json
        buffer.trim() == "ready",
        "Failed to read 'ready' from python process, read: {buffer:?}"
    );
    buffer.clear();

    Ok((child, pid, stdin, stdout, buffer))
}

/// Process a single command with the external process.
///
/// Returns:
/// - Ok(_) on succesful completion
/// - Err(None) if nothing was done
/// - Err(Some(_)) if the process can no longer be reused
async fn process(
    command: Command,
    command_buffer: &mut Vec<u8>,
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    buffer: &mut String,
) -> Result<(Option<Timings>, Status), Option<SubprocessExitReason>> {
    let (call, at_block, mut response) = command;
    command_buffer.clear();

    let cmd = ChildCommand {
        contract_address: &call.contract_address,
        calldata: &call.calldata,
        entry_point_selector: &call.entry_point_selector,
        at_block: &at_block,
    };

    let mut cursor = std::io::Cursor::new(command_buffer);

    if let Err(e) = serde_json::to_writer(&mut cursor, &cmd) {
        error!(command=?cmd, error=%e, "Failed to render command as json");
        let _ = response.send(Err(CallFailure::Internal(
            "Failed to render command as json",
        )));
        return Err(None);
    }

    let command_buffer = cursor.into_inner();

    // using tokio::select to race against the shutdown_rx requires additional block to release
    // the &mut borrow on buffer to have it printed/logged
    let res = {
        // AsyncWriteExt::write_all used in the rpc_round is not cancellation safe, but
        // similar to above, if we lose the race, will kill the subprocess and get out.
        let rpc_op = rpc_round(command_buffer, stdin, stdout, buffer);
        tokio::pin!(rpc_op);

        tokio::select! {
            res = &mut rpc_op => res,
            // no need to await for child dying here, because the event would close the childs
            // stdout and thus break our read_line and thus return a SubprocessError::IO and
            // we'd break out.
            _ = response.closed() => {
                // attempt to guard against a call that essentially freezes up the python for
                // how many minutes. by keeping our eye on this, we'll give the caller a
                // chance to set timeouts, which will drop the futures.
                //
                // breaking out here will end up killing the python. it's probably the safest
                // way to not cancel processing, because you can can't rely on SIGINT not being
                // handled in a `expect Exception:` branch.
                return Err(Some(SubprocessExitReason::Cancellation));
            }
        }
    };

    let (timings, status, sent_response) = match res {
        Ok(resp) => resp.into_messages(),
        Err(SubprocessError::InvalidJson(error)) => {
            // buffer still holds the response... might be good for debugging
            // this doesn't however mess up our line at once, so no worries.
            error!(%error, ?buffer, "Failed to parse json from subprocess");
            (
                None,
                Status::Failed,
                Err(CallFailure::Internal("Invalid json received")),
            )
        }
        Err(SubprocessError::InvalidResponse) => {
            error!(?buffer, "Failed to understand parsed json from subprocess");
            (
                None,
                Status::Failed,
                Err(CallFailure::Internal("Invalid json received")),
            )
        }
        Err(SubprocessError::IO) => {
            let error = CallFailure::Internal("Input/output");
            let _ = response.send(Err(error));

            // TODO: consider if we'd just retry; put this back into the queue?
            return Err(Some(SubprocessExitReason::UnrecoverableIO));
        }
    };

    // This will demo that the span is correctly combined to the callers span, but unnecessary
    // let result = response.send(sent_response).map_err(|_| ());
    // trace!(?result, "Call result sent");

    let _ = response.send(sent_response);

    Ok((timings, status))
}

/// Run a round of writing out the request, and reading a sane response type.
async fn rpc_round<'a>(
    cmd: &[u8],
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut tokio::io::BufReader<tokio::process::ChildStdout>,
    buffer: &'a mut String,
) -> Result<RefinedChildResponse<'a>, SubprocessError> {
    // TODO: using a vectored write here would make most sense, but alas, advancing [IoSlice]'s is
    // still unstable. it could be copied, but we'd still lack `write_vectored_all`.
    //
    // note: write_all are not cancellation safe, and we call this from tokio::select! see callsite
    // for more discussion.
    stdin.write_all(cmd).await?;
    stdin.write_all(&b"\n"[..]).await?;
    stdin.flush().await?;

    // the read buffer is cleared very late to allow logging the output in case of an error.
    buffer.clear();

    let read = stdout.read_line(buffer).await?;

    if read == 0 {
        // EOF
        return Err(SubprocessError::IO);
    }

    let resp =
        serde_json::from_str::<ChildResponse>(buffer).map_err(SubprocessError::InvalidJson)?;

    resp.refine()
}
