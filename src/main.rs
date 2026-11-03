mod child;
mod config;
mod dryrun;
mod exporter;
mod key;
mod metrics;
mod ncsa;
mod otlp;
mod record;
mod span;
mod text;

use clap::Parser;
use exporter::{Counters, SHUTDOWN_GRACE, Shutdown};
use ncsa::Line;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel};
use std::time::{Duration, Instant};

/// Everything the configuration can get wrong, checked before anything starts.
struct Settings {
    exporter: exporter::Config,
    key: Option<key::Key>,
    metrics: Option<TcpListener>,
}

fn settings(cli: &config::Cli) -> Result<Settings, String> {
    let endpoint = cli.endpoint()?;
    let headers = cli.header_pairs()?;
    let compression = cli.compression()?;
    let ca_certificates = cli.ca_certificates()?;
    let key = if cli.stdin {
        None
    } else {
        Some(key::Key::load(&cli.key_file)?)
    };
    let metrics = cli
        .metrics_listen
        .map(|addr| {
            TcpListener::bind(addr).map_err(|err| format!("metrics listener {addr}: {err}"))
        })
        .transpose()?;
    Ok(Settings {
        exporter: exporter::Config {
            endpoint,
            headers,
            batch_size: cli.batch_size as usize,
            batch_timeout: Duration::from_millis(cli.batch_timeout_ms),
            export_timeout: Duration::from_millis(cli.export_timeout_ms),
            service_name: cli.service_name.clone(),
            resource_attributes: cli.resource_attributes(),
            compression,
            ca_certificates,
            dry_run: cli.dry_run,
        },
        key,
        metrics,
    })
}

fn main() -> ExitCode {
    let cli = config::Cli::parse();
    let settings = match settings(&cli) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("varnish-tracer: invalid configuration: {err}");
            return ExitCode::from(2);
        }
    };
    let counters = Arc::new(Counters::default());
    let shutdown = Arc::new(Shutdown::new());
    let slot = child::Slot::default();

    if let Some(listener) = settings.metrics {
        metrics::spawn(listener, counters.clone());
    }
    let (tx, rx) = sync_channel(cli.queue_size as usize);
    // The exporter thread owns `exporter_done`; the receiver disconnects when the thread ends.
    let (exporter_done, exporter_ended) = channel::<()>();
    {
        let (counters, shutdown) = (counters.clone(), shutdown.clone());
        let exporter_cfg = settings.exporter;
        std::thread::spawn(move || {
            let _exporter_done = exporter_done;
            exporter::run(rx, exporter_cfg, &counters, &shutdown);
        });
    }

    let mut child_status = None;
    if let Some(key) = settings.key {
        install_signal_handler(shutdown.clone(), slot.clone());
        let mut spawned = match ncsa::spawn(&cli.varnishncsa, &cli.instance, &key) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("varnish-tracer: cannot start {}: {err}", cli.varnishncsa);
                return ExitCode::from(2);
            }
        };
        let child = Arc::new(child::Child::new(spawned.id()));
        let _ = slot.set(child.clone());
        if shutdown.get().is_some() {
            child::stop(&slot);
        }
        let stderr = spawned.stderr.take().expect("stderr is piped");
        let stderr_counters = counters.clone();
        let stderr_thread = std::thread::spawn(move || {
            ncsa::forward_stderr(BufReader::new(stderr), &stderr_counters);
        });
        let stdout = spawned.stdout.take().expect("stdout is piped");
        pump(BufReader::new(stdout), &tx, &counters, cli.include_query);
        // A read error or a dead exporter ends the pump while varnishncsa may still be running
        child_status = Some(match spawned.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) => {
                child::stop(&slot);
                spawned.wait()
            }
            Err(err) => Err(err),
        });
        child.mark_reaped();
        let _ = stderr_thread.join();
    } else {
        pump(
            BufReader::new(std::io::stdin().lock()),
            &tx,
            &counters,
            cli.include_query,
        );
    }

    drop(tx);
    wait_for_exporter(&exporter_ended, &shutdown, &counters);

    match child_status {
        None => ExitCode::SUCCESS,
        Some(Ok(status)) if shutdown.get().is_some() => {
            eprintln!("varnish-tracer: stopped, varnishncsa {status}");
            ExitCode::SUCCESS
        }
        Some(Ok(status)) => {
            eprintln!("varnish-tracer: varnishncsa exited: {status}");
            ExitCode::FAILURE
        }
        Some(Err(err)) => {
            eprintln!("varnish-tracer: waiting for varnishncsa failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Waits for the exporter to finish its flush; after shutdown it gives up shortly after the grace period
/// and counts the spans it still holds or has queued as `dropped_export`.
fn wait_for_exporter(
    ended: &std::sync::mpsc::Receiver<()>,
    shutdown: &Shutdown,
    counters: &Counters,
) {
    let limit = SHUTDOWN_GRACE + Duration::from_millis(500);
    loop {
        match ended.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) => {
                if shutdown.get().is_some_and(|at| at.elapsed() >= limit) {
                    let lost = counters.abandon();
                    eprintln!(
                        "varnish-tracer: exporter still busy, exiting with {lost} spans dropped"
                    );
                    exporter::report(counters);
                    return;
                }
            }
            _ => return,
        }
    }
}

fn install_signal_handler(shutdown: Arc<Shutdown>, slot: child::Slot) {
    let mut signals = Signals::new([SIGTERM, SIGINT]).expect("signal handler");
    std::thread::spawn(move || {
        for _ in signals.forever() {
            let _ = shutdown.set(Instant::now());
            child::stop(&slot);
        }
    });
}

/// Reads JSON lines until EOF, queueing spans without ever blocking.
fn pump(
    mut reader: impl BufRead,
    tx: &SyncSender<otlp::Span>,
    counters: &Counters,
    include_query: bool,
) {
    let mut line = Vec::new();
    loop {
        match ncsa::read_line(&mut reader, &mut line) {
            Ok(Line::Eof) => return,
            Ok(Line::Complete) => {}
            Ok(Line::TooLong) => {
                counters.malformed.fetch_add(1, Relaxed);
                continue;
            }
            Err(err) => {
                eprintln!("varnish-tracer: read error: {err}");
                return;
            }
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Some(record) = record::parse(&line) else {
            counters.malformed.fetch_add(1, Relaxed);
            continue;
        };
        // Counted before the send, so the exporter never decrements a span that is not counted yet
        counters.queued.fetch_add(1, Relaxed);
        match tx.try_send(span::to_span(&record, include_query)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                counters.queued.fetch_sub(1, Relaxed);
                counters.dropped_queue.fetch_add(1, Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                counters.queued.fetch_sub(1, Relaxed);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn pump_counts_malformed_and_drops_when_full() {
        let good =
            r#"{"tp":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01","start":"1.5"}"#;
        let input = format!("{good}\n\nnot json\n{good}\n{good}\n");
        let (tx, rx) = sync_channel(2);
        let counters = Counters::default();
        pump(input.as_bytes(), &tx, &counters, false);
        assert_eq!(rx.try_iter().count(), 2);
        assert_eq!(counters.malformed.load(Relaxed), 1);
        assert_eq!(counters.dropped_queue.load(Relaxed), 1);
    }

    #[test]
    fn pump_survives_invalid_utf8_and_counts_long_lines() {
        let mut input = br#"{"tp":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01","start":"1.5","path":"/a"#.to_vec();
        input.extend(b"\xff\"}\n");
        input.extend(vec![b'x'; ncsa::MAX_LINE_BYTES + 1]);
        input.push(b'\n');
        let (tx, rx) = sync_channel(4);
        let counters = Counters::default();
        pump(input.as_slice(), &tx, &counters, false);
        assert_eq!(rx.try_iter().count(), 1);
        assert_eq!(counters.malformed.load(Relaxed), 1);
    }

    #[test]
    fn giving_up_on_the_exporter_counts_the_spans_it_still_holds() {
        let counters = Counters::default();
        counters.held.store(4, Relaxed);
        counters.queued.store(6, Relaxed);
        // The sender is never dropped, so the exporter looks busy for ever
        let (_exporter_done, ended) = channel::<()>();
        let shutdown = Shutdown::new();
        let long_ago = Instant::now() - SHUTDOWN_GRACE - Duration::from_secs(1);
        shutdown.set(long_ago).unwrap();
        let started = Instant::now();
        wait_for_exporter(&ended, &shutdown, &counters);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(counters.dropped_export.load(Relaxed), 10);
        assert_eq!(counters.held.load(Relaxed), 0);
        assert_eq!(counters.queued.load(Relaxed), 0);
    }

    #[test]
    fn pump_tracks_the_queue_length() {
        let good =
            r#"{"tp":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01","start":"1.5"}"#;
        let input = format!("{good}\n{good}\n{good}\n");
        let (tx, rx) = sync_channel(2);
        let counters = Counters::default();
        pump(input.as_bytes(), &tx, &counters, false);
        assert_eq!(counters.queued.load(Relaxed), 2);
        assert_eq!(counters.dropped_queue.load(Relaxed), 1);
        drop(rx);
        pump(input.as_bytes(), &tx, &counters, false);
        assert_eq!(counters.queued.load(Relaxed), 2);
    }
}
