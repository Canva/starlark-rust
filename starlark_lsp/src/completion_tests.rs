//! Integration tests for member (dot) completion, driven through a real
//! in-process LSP server. See `test.rs` for the harness.

use std::str::FromStr;

use lsp_types::CompletionParams;
use lsp_types::CompletionResponse;
use lsp_types::PartialResultParams;
use lsp_types::Position;
use lsp_types::TextDocumentIdentifier;
use lsp_types::TextDocumentPositionParams;
use lsp_types::Uri;
use lsp_types::WorkDoneProgressParams;
use lsp_types::request::Completion;

use crate::test::TestServer;

const HELPERS: &str = r#"
def _render(config):
    """Render the config."""
    pass

def _validate(config):
    pass

helpers = struct(
    render = _render,
    validate = _validate,
)
"#;

fn completion_labels(server: &mut TestServer, uri: &Uri, line: u32, character: u32) -> Vec<String> {
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };
    let req = server.new_request::<Completion>(params);
    let id = server.send_request(req).unwrap();
    match server.get_response::<CompletionResponse>(id).unwrap() {
        CompletionResponse::Array(items) => {
            let mut labels: Vec<String> = items.into_iter().map(|item| item.label).collect();
            labels.sort();
            labels
        }
        CompletionResponse::List(_) => panic!("expected an array completion response"),
    }
}

/// Open a document that loads the helpers struct, then edit it into the given
/// (possibly unparseable) state, as an editor would.
fn server_with_document(final_content: &str) -> (TestServer, Uri) {
    let mut server = TestServer::new().unwrap();
    let helpers_uri = Uri::from_str("file:///dir/helpers.star").unwrap();
    let test_uri = Uri::from_str("file:///dir/test.star").unwrap();
    server
        .set_file_contents(&helpers_uri, HELPERS.to_owned())
        .unwrap();
    server
        .open_file(
            test_uri.clone(),
            "load(\"helpers.star\", \"helpers\")\nx = helpers\n".to_owned(),
        )
        .unwrap();
    server
        .change_file(test_uri.clone(), final_content.to_owned())
        .unwrap();
    (server, test_uri)
}

#[test]
fn test_completion_trigger_characters_include_dot() {
    let server = TestServer::new().unwrap();
    let capabilities = server.initialization_result().unwrap().capabilities;
    let completion = capabilities.completion_provider.unwrap();
    assert_eq!(completion.trigger_characters, Some(vec![".".to_owned()]));
}

#[test]
fn test_member_completion_on_unparseable_dot_state() {
    // `x = helpers.` does not parse; members must come from the latest text +
    // the stale AST's load statements.
    let (mut server, uri) =
        server_with_document("load(\"helpers.star\", \"helpers\")\nx = helpers.\n");
    assert_eq!(
        completion_labels(&mut server, &uri, 1, 12),
        vec!["render".to_owned(), "validate".to_owned()]
    );
}

#[test]
fn test_member_completion_with_partial_word() {
    let (mut server, uri) =
        server_with_document("load(\"helpers.star\", \"helpers\")\nx = helpers.re\n");
    assert_eq!(
        completion_labels(&mut server, &uri, 1, 14),
        vec!["render".to_owned(), "validate".to_owned()]
    );
}

#[test]
fn test_member_completion_unresolvable_root_is_empty() {
    // Deliberate: no fallback to default completions after a dot.
    let (mut server, uri) =
        server_with_document("load(\"helpers.star\", \"helpers\")\nx = nope.\n");
    assert_eq!(
        completion_labels(&mut server, &uri, 1, 9),
        Vec::<String>::new()
    );
}

#[test]
fn test_member_completion_chained_access_is_empty() {
    let (mut server, uri) =
        server_with_document("load(\"helpers.star\", \"helpers\")\ny = helpers.render.\n");
    // The final dot is at column 18; the cursor sits after it at column 19.
    assert_eq!(
        completion_labels(&mut server, &uri, 1, 19),
        Vec::<String>::new()
    );
}

#[test]
fn test_member_completion_respects_load_rename() {
    // load("helpers.star", h = "helpers") binds local `h` to remote `helpers`.
    let mut server = TestServer::new().unwrap();
    let helpers_uri = Uri::from_str("file:///dir/helpers.star").unwrap();
    let test_uri = Uri::from_str("file:///dir/test.star").unwrap();
    server
        .set_file_contents(&helpers_uri, HELPERS.to_owned())
        .unwrap();
    server
        .open_file(
            test_uri.clone(),
            "load(\"helpers.star\", h = \"helpers\")\nx = h\n".to_owned(),
        )
        .unwrap();
    server
        .change_file(
            test_uri.clone(),
            "load(\"helpers.star\", h = \"helpers\")\nx = h.\n".to_owned(),
        )
        .unwrap();
    assert_eq!(
        completion_labels(&mut server, &test_uri, 1, 6),
        vec!["render".to_owned(), "validate".to_owned()]
    );
}
