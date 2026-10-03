//! `get_board_extents` through served `tools/call` (#688).
//!
//! The KiCad double answers `GetBoundingBox` the way KiCad 10.0.5 does: one box
//! for each KIID a request names, and none for an empty request. The old read
//! sent an empty request, so against this double it measures nothing, which is
//! what it measured against every real board.
//!
//! The saved half runs on KiCad's own `ecc83-pp` demo board, and its expected
//! extent is KiCad's live answer for that board, captured over IPC (see
//! `konnect-sexp/tests/fixtures/board_bounds/`). The live half shares no
//! coordinate with it, so a test reading the wrong source fails on a value.

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::CallToolResult;
use crate::test_support::MockIpcServer;
use crate::tools::pcb_board::board_mock::{
    board_document, kicad_bounding_boxes, listed_item, spawn_kicad_holding_board,
};
use crate::tools::ServerConfig;
use konnect_ipc::gen::kiapi;
use konnect_ipc::gen::kiapi::common::types::KiCadObjectType as Kind;
use prost::Message;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// KiCad 10.0.5's extent of `ecc83-pp.kicad_pcb`, as the union of its live
/// `GetBoundingBox` answers: `(x_min, y_min, x_max, y_max)` in mm.
const ECC83_KICAD_EXTENT: (f64, f64, f64, f64) = (121.2215, 90.1065, 173.4185, 137.966);

/// One item the double holds: its class, its KIID, and its box
/// `(x, y, width, height)` in mm.
type LiveItem = (Kind, &'static str, (f64, f64, f64, f64));

/// What the double holds instead: one footprint, one graphic and one track,
/// with boxes nowhere near the saved board.
const LIVE_ITEMS: [LiveItem; 3] = [
    (
        Kind::KotPcbFootprint,
        "live-footprint",
        (200.0, 150.0, 10.0, 8.0),
    ),
    (
        Kind::KotPcbShape,
        "live-outline",
        (190.0, 140.0, 40.0, 30.0),
    ),
    (Kind::KotPcbTrace, "live-track", (205.0, 152.0, 1.0, 1.0)),
];

fn fixture_board(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../konnect-sexp/tests/fixtures/ecc83-pp.kicad_pcb");
    let board = dir.join("ecc83-pp.kicad_pcb");
    std::fs::copy(&source, &board).expect("the ecc83 fixture is committed");
    board
}

/// A KiCad holding `open` with `items` on it, `(class, KIID, box)`.
fn spawn_kicad_with_items(open: &Path, items: Vec<LiveItem>) -> MockIpcServer {
    spawn_kicad_holding_board(open, move |command| {
        if command.type_url.ends_with("GetItems") {
            let request = kiapi::common::commands::GetItems::decode(command.value.as_slice())
                .expect("a GetItems request");
            let listed_items = items
                .iter()
                .filter(|(kind, _, _)| request.types.contains(&(*kind as i32)))
                .map(|(kind, kiid, _)| listed_item(*kind, kiid))
                .collect();
            return Some(konnect_ipc::builders::pack_any(
                &kiapi::common::commands::GetItemsResponse {
                    header: None,
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    items: listed_items,
                },
                "kiapi.common.commands.GetItemsResponse",
            ));
        }
        if command.type_url.ends_with("GetBoundingBox") {
            let items = items.clone();
            return Some(kicad_bounding_boxes(command, move |kiid| {
                items
                    .iter()
                    .find(|(_, id, _)| *id == kiid)
                    .map(|(_, _, bounds)| *bounds)
                    .unwrap_or_else(|| panic!("asked to measure an unknown KIID {kiid}"))
            }));
        }
        None
    })
}

/// A KiCad holding `open` that refuses to list `refused`, the way it refuses a
/// type it does not serve: `AS_BAD_REQUEST` on the `GetItems` round trip.
fn spawn_kicad_refusing_to_list(open: &Path, refused: Kind) -> MockIpcServer {
    let documents = vec![board_document(&open.to_string_lossy())];
    MockIpcServer::spawn("extents-refusing-class", move |request| {
        let command = request.message.expect("a command");
        let ok = |message| kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsOk as i32,
                error_message: String::new(),
            }),
            header: None,
            message,
        };
        if command.type_url.ends_with("GetOpenDocuments") {
            return ok(Some(konnect_ipc::builders::pack_any(
                &kiapi::common::commands::GetOpenDocumentsResponse {
                    documents: documents.clone(),
                },
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )));
        }
        let asked = kiapi::common::commands::GetItems::decode(command.value.as_slice())
            .expect("only GetItems follows the binding");
        if asked.types.contains(&(refused as i32)) {
            return kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                    error_message: "none of the requested types are valid for a Board object"
                        .to_string(),
                }),
                header: None,
                message: None,
            };
        }
        ok(Some(konnect_ipc::builders::pack_any(
            &kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: Vec::new(),
            },
            "kiapi.common.commands.GetItemsResponse",
        )))
    })
}

struct Scene {
    _dir: tempfile::TempDir,
    board: PathBuf,
    _kicad: Option<MockIpcServer>,
    handler: McpHandler,
}

impl Scene {
    async fn build(kicad: impl FnOnce(&Path) -> Option<MockIpcServer>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_board(dir.path());
        let kicad = kicad(&board);
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: kicad.as_ref().map_or("", |k| k.address()).to_string(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        Self {
            _dir: dir,
            board,
            _kicad: kicad,
            handler,
        }
    }

    async fn holding(items: Vec<LiveItem>) -> Self {
        Self::build(move |board| Some(spawn_kicad_with_items(board, items))).await
    }

    async fn call(&self, board_source: Option<&str>) -> CallToolResult {
        let mut arguments = json!({ "board": self.board.to_string_lossy() });
        if let Some(mode) = board_source {
            arguments["board_source"] = json!(mode);
        }
        let response = self
            .handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 688, "method": "tools/call",
                "params": { "name": "get_board_extents", "arguments": arguments }
            }))
            .await
            .expect("tools/call receives a response");
        assert!(response.error.is_none(), "tool errors are MCP results");
        serde_json::from_value(response.result.expect("a result")).expect("an MCP result")
    }

    async fn body(&self, board_source: Option<&str>) -> Value {
        let result = self.call(board_source).await;
        assert!(!result.is_error, "{:?}", body_text(&result));
        serde_json::from_str(&body_text(&result)).unwrap()
    }
}

fn body_text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn extent(body: &Value) -> (f64, f64, f64, f64) {
    let f = |key: &str| {
        body[key]
            .as_f64()
            .unwrap_or_else(|| panic!("{key}: {body}"))
    };
    (f("x_min"), f("y_min"), f("x_max"), f("y_max"))
}

fn assert_close(actual: (f64, f64, f64, f64), expected: (f64, f64, f64, f64)) {
    let off = (actual.0 - expected.0)
        .abs()
        .max((actual.1 - expected.1).abs())
        .max((actual.2 - expected.2).abs())
        .max((actual.3 - expected.3).abs());
    assert!(off < 1e-6, "extent {actual:?}, expected {expected:?}");
}

/// The live board is measured item by item, so a KiCad that answers only for
/// the KIIDs it is asked about still yields its extent.
#[tokio::test]
async fn the_live_answer_measures_every_item_kicad_lists() {
    let scene = Scene::holding(LIVE_ITEMS.to_vec()).await;
    let body = scene.body(None).await;
    assert_close(extent(&body), (190.0, 140.0, 230.0, 170.0));
    assert_eq!(body["width"], json!(40.0));
    assert_eq!(body["height"], json!(30.0));
    assert_eq!(body["item_count"], json!(3));
    assert_eq!(
        body["measured_item_counts"],
        json!({ "footprints": 1, "shapes": 1, "tracks": 1 })
    );
    assert_eq!(body["unmeasured_item_counts"], json!({}));
    assert_eq!(body["shared_kiid_count"], json!(0));
    assert_eq!(body["source"], json!("ipc"));
    assert_eq!(body["sources"], json!({ "bounds": "ipc" }));
    assert_eq!(body["source_evidence"]["board_state"], json!("ipc"));
}

/// The saved board is measured as KiCad measures it: its extent is KiCad's own
/// answer for the same file, not the anchors and lines the old read collected.
#[tokio::test]
async fn the_saved_answer_is_kicads_extent_of_the_file() {
    let scene = Scene::holding(LIVE_ITEMS.to_vec()).await;
    let body = scene.body(Some("saved")).await;
    assert_close(extent(&body), ECC83_KICAD_EXTENT);
    assert_eq!(body["item_count"], json!(79));
    assert_eq!(
        body["measured_item_counts"],
        json!({ "footprints": 15, "shapes": 4, "tracks": 59, "zones": 1 })
    );
    assert_eq!(body["unmeasured_item_counts"], json!({}));
    assert_eq!(body["skipped_item_count"], json!(0));
    assert_eq!(body["source"], json!("file"));
    assert_eq!(body["sources"], json!({ "bounds": "saved_board" }));
    assert_eq!(
        body["source_evidence"]["excludes_unsaved_editor_state"],
        json!(true)
    );
}

/// Unreachable KiCad: the file answers, and the response says why.
#[tokio::test]
async fn without_kicad_the_file_answers_and_says_why() {
    let scene = Scene::build(|_| None).await;
    let body = scene.body(None).await;
    assert_close(extent(&body), ECC83_KICAD_EXTENT);
    assert_eq!(body["sources"], json!({ "bounds": "saved_board" }));
    assert_eq!(
        body["source_evidence"]["reason"],
        json!("kicad_ipc_unreachable")
    );
}

/// KiCad holds the board and refuses to list a class of its items. The read
/// fails by name rather than answering from the file, which may be older than
/// the editor; the old read discarded this failure and answered from the file.
#[tokio::test]
async fn a_class_kicad_refuses_to_list_fails_the_read_by_name() {
    let scene =
        Scene::build(|board| Some(spawn_kicad_refusing_to_list(board, Kind::KotPcbDimension)))
            .await;
    let result = scene.call(None).await;
    assert!(result.is_error, "{}", body_text(&result));
    let text = body_text(&result);
    assert!(text.contains("dimensions"), "{text}");
    assert_eq!(
        crate::mcp::error::extract_error_kind(&result).as_deref(),
        Some("editor_unavailable"),
        "{text}"
    );
}

/// A board with no items has no extent, and says so with nulls rather than a
/// zero box that reads as a board drawn at the origin.
#[tokio::test]
async fn an_empty_board_has_no_extent_rather_than_a_box_at_the_origin() {
    let scene = Scene::holding(Vec::new()).await;
    let body = scene.body(None).await;
    for key in ["x_min", "y_min", "x_max", "y_max", "width", "height"] {
        assert_eq!(body[key], Value::Null, "{key}: {body}");
    }
    assert_eq!(body["item_count"], json!(0));
    assert_eq!(body["measured_item_counts"], json!({}));
}

/// KiCad can list one KIID twice. It is measured once and the response counts
/// the repeat, instead of the whole read failing on a duplicate request ID.
#[tokio::test]
async fn a_kiid_kicad_lists_twice_is_measured_once_and_counted() {
    let mut items = LIVE_ITEMS.to_vec();
    items.push((Kind::KotPcbTrace, "live-track", (205.0, 152.0, 1.0, 1.0)));
    let scene = Scene::holding(items).await;
    let body = scene.body(None).await;
    assert_close(extent(&body), (190.0, 140.0, 230.0, 170.0));
    assert_eq!(body["item_count"], json!(4));
    assert_eq!(body["shared_kiid_count"], json!(1));
}
