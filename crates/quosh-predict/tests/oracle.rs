//! Differential test: replay the same keystroke/echo/ack stream through the
//! Rust predictor and through Mosh's PredictionEngine.
//!
//! Scenarios live in `tools/mosh-oracle/scenarios/`; golden traces in
//! `tests/fixtures/` are produced by `tools/mosh-oracle/generate.sh`. The
//! authoritative frame here is the same `blit-alacritty` emulator the server
//! uses, fed the same bytes the oracle fed Mosh's emulator.
//!
//! The reference checkout is not vendored, so the fixtures are committed and
//! this test runs anywhere. Regenerate only when the port intentionally
//! changes behaviour, and explain why in the commit.

use blit_remote::{CELL_SIZE, FrameState};
use quosh_predict::{DisplayPreference, Predictor};
use quosh_server::term::Emulator;
use std::path::PathBuf;

const ROWS: u16 = 24;
const COLS: u16 = 80;

#[derive(Debug)]
struct Grid {
    cur: (u16, u16),
    rows: Vec<(String, String)>,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn parse_golden(text: &str) -> Vec<Grid> {
    let mut grids = Vec::new();
    let mut cur = None;
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut pending_row: Option<(usize, String)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("CUR ") {
            let mut it = rest.split_whitespace();
            cur = Some((
                it.next().unwrap().parse().unwrap(),
                it.next().unwrap().parse().unwrap(),
            ));
        } else if let Some(rest) = line.strip_prefix("ROW ") {
            let (idx, content) = rest.split_once(' ').unwrap();
            pending_row = Some((idx.parse().unwrap(), content.to_string()));
        } else if let Some(rest) = line.strip_prefix("UL  ") {
            let (idx, bitmap) = rest.split_once(' ').unwrap();
            let (row_idx, content) = pending_row.take().expect("UL before ROW");
            assert_eq!(row_idx, idx.parse::<usize>().unwrap());
            rows.push((content, bitmap.to_string()));
        } else if line == "END" {
            grids.push(Grid {
                cur: cur.take().expect("CUR"),
                rows: std::mem::take(&mut rows),
            });
        }
    }
    grids
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

fn render(frame: &FrameState) -> Grid {
    let mut rows = Vec::with_capacity(frame.rows() as usize);
    for r in 0..frame.rows() {
        let mut text = String::new();
        let mut ul = String::with_capacity(frame.cols() as usize);
        for c in 0..frame.cols() {
            text.push_str(frame.cell_content(r, c));
            let i = (r as usize * frame.cols() as usize + c as usize) * CELL_SIZE;
            ul.push(if frame.cells()[i] & (1 << 7) != 0 {
                '1'
            } else {
                '0'
            });
        }
        rows.push((text, ul));
    }
    Grid {
        cur: (frame.cursor_row(), frame.cursor_col()),
        rows,
    }
}

fn assert_grid(got: &Grid, want: &Grid, step: &str) {
    assert_eq!(got.cur, want.cur, "{step}: cursor");
    for (r, ((gt, gu), (wt, wu))) in got.rows.iter().zip(&want.rows).enumerate() {
        assert_eq!(gt, wt, "{step}: row {r} text");
        assert_eq!(gu, wu, "{step}: row {r} underline");
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_step(
    emu: &mut Emulator,
    pred: &mut Predictor,
    local: &mut FrameState,
    now: u64,
    golden: &[Grid],
    gi: &mut usize,
    name: &str,
) {
    let mut shown = emu.snapshot();
    pred.cull(&shown, now);
    pred.apply(&mut shown);
    assert!(*gi < golden.len(), "{name}: more renders than golden grids");
    assert_grid(&render(&shown), &golden[*gi], name);
    *gi += 1;
    *local = shown;
}

fn run_scenario(name: &str) {
    let scenario_path = root().join(format!("tools/mosh-oracle/scenarios/{name}.txt"));
    let golden_path = root().join(format!("crates/quosh-predict/tests/fixtures/{name}.golden"));
    let scenario = std::fs::read_to_string(&scenario_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", scenario_path.display()));
    let golden = parse_golden(&std::fs::read_to_string(&golden_path).expect("read golden fixture"));

    let mut emu = Emulator::new(ROWS, COLS, 2000);
    let mut pred = Predictor::new();
    pred.set_display_preference(DisplayPreference::Adaptive);
    pred.set_send_interval(250);
    let mut local = emu.snapshot();
    let mut sent: u64 = 0;
    let mut now: u64 = 0;
    let mut gi = 0;

    for (lineno, raw) in scenario.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (cmd, arg) = match line.split_once(' ') {
            Some((c, a)) => (c, a.trim()),
            None => (line, ""),
        };
        let step = format!("{name}:{} `{line}`", lineno + 1);
        match cmd {
            "FEED" => {
                emu.process(&unhex(arg));
                compare_step(
                    &mut emu, &mut pred, &mut local, now, &golden, &mut gi, &step,
                );
            }
            "KEY" => {
                for b in unhex(arg) {
                    pred.set_local_frame_sent(sent);
                    sent += 1;
                    pred.new_user_byte(b, &local, now);
                }
                compare_step(
                    &mut emu, &mut pred, &mut local, now, &golden, &mut gi, &step,
                );
            }
            "TICK" => {
                now += arg.parse::<u64>().unwrap();
                compare_step(
                    &mut emu, &mut pred, &mut local, now, &golden, &mut gi, &step,
                );
            }
            "SENT" => {
                sent = arg.parse().unwrap();
                pred.set_local_frame_sent(sent);
            }
            "EARLY" => pred.set_local_frame_acked(arg.parse().unwrap()),
            "LATE" => pred.set_local_frame_late_acked(arg.parse().unwrap()),
            "RTT" => pred.set_send_interval(arg.parse().unwrap()),
            "RESET" => pred.reset(),
            "PRED" => pred.set_display_preference(match arg {
                "always" => DisplayPreference::Always,
                "never" => DisplayPreference::Never,
                _ => DisplayPreference::Adaptive,
            }),
            other => panic!("{step}: unknown command {other}"),
        }
    }
    assert_eq!(gi, golden.len(), "{name}: not all golden grids consumed");
}

#[test]
fn oracle_basic() {
    run_scenario("basic");
}

#[test]
fn oracle_cursor() {
    run_scenario("cursor");
}

#[test]
fn oracle_unicode() {
    run_scenario("unicode");
}

#[test]
fn oracle_cr() {
    run_scenario("cr");
}

#[test]
fn oracle_echo_off() {
    run_scenario("echo_off");
}
