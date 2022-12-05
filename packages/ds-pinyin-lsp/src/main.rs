use dashmap::DashMap;
use ds_pinyin_lsp::types::Setting;
use ds_pinyin_lsp::utils::{
    get_pinyin, get_pre_line, query_dict, query_words, suggest_to_completion_item,
};
use lsp_document::{apply_change, IndexedText, TextAdapter};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct Backend {
    client: Client,
    setting: Mutex<Option<Setting>>,
    conn: Mutex<Option<Connection>>,
    documents: DashMap<String, IndexedText<String>>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.init(&params.initialization_options).await;

        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: Some(vec![]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                }),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.documents.insert(
            params.text_document.uri.to_string(),
            IndexedText::new(params.text_document.text),
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut document = self
            .documents
            .entry(params.text_document.uri.to_string())
            .or_insert(IndexedText::new(String::new()));

        let mut content: String;

        for change in params.content_changes {
            if let Some(change) = document.lsp_change_to_change(change) {
                content = apply_change(&document, change);
                *document = IndexedText::new(content);
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();

        // remove close document
        self.documents.remove(&uri);

        self.client
            .log_message(MessageType::INFO, &format!("Close file: {}", &uri))
            .await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri.to_string();
        let document = self.documents.get(&uri);
        let pre_line = get_pre_line(&document, &position).unwrap_or("");

        if pre_line.is_empty() {
            return Ok(Some(CompletionResponse::Array(vec![])));
        }

        let pinyin = get_pinyin(pre_line).unwrap_or(String::new());

        if pinyin.is_empty() {
            return Ok(Some(CompletionResponse::Array(vec![])));
        }

        let range = Range::new(
            Position {
                line: position.line,
                character: position.character - pinyin.len() as u32,
            },
            position,
        );

        if let Some(ref conn) = *self.conn.lock().await {
            // words match
            if let Ok(suggest) = query_words(conn, &pinyin, true) {
                if suggest.len() > 0 {
                    return Ok(Some(CompletionResponse::List(CompletionList {
                        is_incomplete: true,
                        items: suggest_to_completion_item(suggest, range),
                    })));
                }
            }

            // words search
            if let Ok(suggest) = query_words(conn, &pinyin, false) {
                if suggest.len() > 0 {
                    return Ok(Some(CompletionResponse::List(CompletionList {
                        is_incomplete: true,
                        items: suggest_to_completion_item(suggest, range),
                    })));
                }
            }

            // dict search
            if let Ok(suggest) = query_dict(conn, &pinyin) {
                if suggest.len() > 0 {
                    return Ok(Some(CompletionResponse::List(CompletionList {
                        is_incomplete: true,
                        items: suggest_to_completion_item(suggest, range),
                    })));
                }
            }
        };

        Ok(Some(CompletionResponse::Array(vec![])))
    }
}

impl Backend {
    async fn init(&self, initialization_options: &Option<Value>) {
        if let Some(params) = initialization_options {
            let mut setting = self.setting.lock().await;

            let db_path = &Value::String(String::new());

            let db_path = params.get("db-path").unwrap_or(&db_path);

            // invalid db_path
            if !db_path.is_string() {
                return self
                    .client
                    .show_message(MessageType::ERROR, "ds-pinyin-lsp db-path must be string!")
                    .await;
            }

            if let Some(db_path) = db_path.as_str() {
                // db_path missing
                if db_path.is_empty() {
                    return self
                        .client
                        .show_message(
                            MessageType::ERROR,
                            "ds-pinyin-lsp db-path is missing or empty!",
                        )
                        .await;
                }

                // cache setting
                *setting = Some(Setting {
                    db_path: db_path.to_string(),
                });

                // open db connection
                let conn = Connection::open(db_path);
                if let Ok(conn) = conn {
                    let mut mutex = self.conn.lock().await;
                    *mutex = Some(conn);
                    return self
                        .client
                        .log_message(
                            MessageType::INFO,
                            "ds-pinyin-lsp db connection initialized!",
                        )
                        .await;
                } else if let Err(err) = conn {
                    return self
                        .client
                        .show_message(MessageType::ERROR, &format!("Open database error: {}", err))
                        .await;
                }
            }
        } else {
            return self
                .client
                .show_message(
                    MessageType::ERROR,
                    "ds-pinyin-lsp initialization_options is missing, it must include db-path setting!",
                )
                .await;
        }
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| Backend {
        client,
        setting: Mutex::new(None),
        conn: Mutex::new(None),
        documents: DashMap::new(),
    })
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}
