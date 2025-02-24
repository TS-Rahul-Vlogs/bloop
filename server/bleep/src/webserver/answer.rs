use rand::{rngs::OsRng, seq::SliceRandom};
use secrecy::ExposeSecret;
use std::{
    collections::{HashMap, HashSet},
    mem,
    ops::Range,
    panic::AssertUnwindSafe,
    pin::pin,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::Query,
    response::{
        sse::{self, Sse},
        IntoResponse,
    },
    Extension, Json,
};
use futures::{future::Either, stream, StreamExt, TryStreamExt};
use reqwest::StatusCode;
use serde_json::json;
use tiktoken_rs::CoreBPE;
use tokio::sync::mpsc::Sender;
use tracing::{debug, info, warn};

use super::middleware::User;
use crate::{
    analytics::{EventData, QueryEvent},
    db::QueryLog,
    indexes::reader::{ContentDocument, FileDocument},
    query::parser::{self, Literal, SemanticQuery},
    repo::RepoRef,
    semantic, Application,
};

pub mod conversations;
mod exchange;
mod llm_gateway;
mod prompts;

use exchange::{Exchange, SearchStep, Update};
use llm_gateway::api::FunctionCall;

const TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Vote {
    pub feedback: VoteFeedback,
    pub thread_id: uuid::Uuid,
    pub query_id: uuid::Uuid,
    pub repo_ref: Option<RepoRef>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum VoteFeedback {
    Positive,
    Negative { feedback: String },
}

pub(super) async fn vote(
    Extension(app): Extension<Application>,
    Extension(user): Extension<User>,
    Json(params): Json<Vote>,
) {
    app.track_query(
        &user,
        &QueryEvent {
            query_id: params.query_id,
            thread_id: params.thread_id,
            repo_ref: params.repo_ref,
            data: EventData::output_stage("vote").with_payload("feedback", params.feedback),
        },
    );
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Params {
    pub q: String,
    pub repo_ref: RepoRef,
    #[serde(default = "default_thread_id")]
    pub thread_id: uuid::Uuid,
    /// Optional id of the parent of the exchange to overwrite
    /// If this UUID is nil, then overwrite the first exchange in the thread
    pub parent_exchange_id: Option<uuid::Uuid>,
}

fn default_thread_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

pub(super) async fn handle(
    Query(params): Query<Params>,
    Extension(app): Extension<Application>,
    Extension(user): Extension<User>,
) -> super::Result<impl IntoResponse> {
    let query_id = uuid::Uuid::new_v4();
    let response = _handle(
        Query(params.clone()),
        Extension(app.clone()),
        Extension(user.clone()),
        query_id,
    )
    .await;

    if let Err(err) = response.as_ref() {
        app.track_query(
            &user,
            &QueryEvent {
                query_id,
                thread_id: params.thread_id,
                repo_ref: Some(params.repo_ref.clone()),
                data: EventData::output_stage("error")
                    .with_payload("status", err.status.as_u16())
                    .with_payload("message", err.message()),
            },
        );
    }

    response
}

pub(super) async fn _handle(
    Query(params): Query<Params>,
    Extension(app): Extension<Application>,
    Extension(user): Extension<User>,
    query_id: uuid::Uuid,
) -> super::Result<
    Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<sse::Event>> + Send>>>,
> {
    QueryLog::new(&app.sql).insert(&params.q).await?;

    let conversation_id = conversations::ConversationId {
        user_id: user
            .login()
            .ok_or_else(|| super::Error::user("didn't have user ID"))?
            .to_string(),
        thread_id: params.thread_id,
    };

    let (repo_ref, mut exchanges) = conversations::load(&app.sql, &conversation_id)
        .await?
        .unwrap_or_else(|| (params.repo_ref.clone(), Vec::new()));

    let gh_token = app
        .github_token()
        .map_err(|e| super::Error::user(e).with_status(StatusCode::UNAUTHORIZED))?
        .map(|s| s.expose_secret().clone());

    let llm_gateway = llm_gateway::Client::new(&app.config.answer_api_url)
        .temperature(0.0)
        .bearer(gh_token)
        .session_reference_id(conversation_id.to_string());

    // confirm client compatibility with answer-api
    match llm_gateway
        .is_compatible(env!("CARGO_PKG_VERSION").parse().unwrap())
        .await
    {
        Ok(res) if res.status() == StatusCode::OK => (),
        Ok(res) if res.status() == StatusCode::NOT_ACCEPTABLE => {
            let out_of_date = futures::stream::once(async {
                Ok(sse::Event::default()
                    .json_data(serde_json::json!({"Err": "incompatible client"}))
                    .unwrap())
            });
            return Ok(Sse::new(Box::pin(out_of_date)));
        }
        // the Ok(_) case should be unreachable
        Ok(_) | Err(_) => {
            warn!("failed to check compatibility ... defaulting to `incompatible`");
            let failed_to_check = futures::stream::once(async {
                Ok(sse::Event::default()
                    .json_data(serde_json::json!({"Err": "failed to check compatibility"}))
                    .unwrap())
            });
            return Ok(Sse::new(Box::pin(failed_to_check)));
        }
    };

    let Params {
        thread_id,
        parent_exchange_id,
        q,
        ..
    } = params;

    if let Some(parent_exchange_id) = parent_exchange_id {
        let truncate_from_index = if parent_exchange_id.is_nil() {
            0
        } else {
            exchanges
                .iter()
                .position(|e| e.id == parent_exchange_id)
                .ok_or_else(|| super::Error::user("parent query id not found in exchanges"))?
                + 1
        };

        exchanges.truncate(truncate_from_index);
    }

    let query = parser::parse_nl(&q)
        .context("parse error")?
        .into_semantic()
        .context("got a 'Grep' query")?
        .into_owned();
    let query_target = query
        .target
        .as_ref()
        .context("query was empty")?
        .as_plain()
        .context("user query was not plain text")?
        .clone()
        .into_owned();

    exchanges.push(Exchange::new(query_id, query));

    let stream = async_stream::try_stream! {
        let mut action = Action::Query(query_target);
        let (exchange_tx, exchange_rx) = tokio::sync::mpsc::channel(10);

        let mut agent = Agent {
            app,
            repo_ref,
            exchanges,
            exchange_tx,
            llm_gateway,
            user,
            thread_id,
            query_id,
            complete: false,
        };

        let mut exchange_rx = tokio_stream::wrappers::ReceiverStream::new(exchange_rx);

        let result = 'outer: loop {
            // The main loop. Here, we create two streams that operate simultaneously; the update
            // stream, which sends updates back to the HTTP event stream response, and the action
            // stream, which returns a single item when there is a new action available to execute.
            // Both of these operate together, and we repeat the process for every new action.

            use futures::future::FutureExt;

            let left_stream = (&mut exchange_rx).map(Either::Left);
            let right_stream = agent
                .step(action)
                .into_stream()
                .map(Either::Right);

            let timeout = Duration::from_secs(TIMEOUT_SECS);

            let mut next = None;
            for await item in tokio_stream::StreamExt::timeout(
                stream::select(left_stream, right_stream),
                timeout,
            ) {
                match item {
                    Ok(Either::Left(exchange)) => yield exchange.compressed(),
                    Ok(Either::Right(next_action)) => match next_action {
                        Ok(n) => break next = n,
                        Err(e) => break 'outer Err(AgentError::Processing(e)),
                    },
                    Err(_) => break 'outer Err(AgentError::Timeout(timeout)),
                }
            }

            // NB: Sending updates after all other `await` points in the final `step` call will
            // likely not return a pending future due to the internal receiver queue. So, the call
            // stack usually continues onwards, ultimately resulting in a `Poll::Ready`, backing out
            // of the above loop without ever processing the final message. Here, we empty the
            // queue.
            while let Some(Some(exchange)) = exchange_rx.next().now_or_never() {
                yield exchange.compressed();
            }

            match next {
                Some(a) => action = a,
                None => break Ok(()),
            }
        };

        match result {
            Ok(_) => {}
            Err(AgentError::Timeout(duration)) => {
                warn!("Timeout reached.");
                agent.track_query(
                    EventData::output_stage("error")
                        .with_payload("timeout", duration.as_secs()),
                );
                Err(anyhow!("reached timeout of {duration:?}"))?;
            }
            Err(AgentError::Processing(e)) => {
                agent.track_query(
                    EventData::output_stage("error")
                        .with_payload("message", e.to_string()),
                );
                Err(e)?;
            }
        }

        // Storing the conversation here allows us to make subsequent requests.
        conversations::store(&agent.app.sql, conversation_id, (agent.repo_ref.clone(), agent.exchanges.clone())).await?;
        agent.complete();
    };

    let init_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!({
                "thread_id": params.thread_id.to_string(),
                "query_id": query_id
            }))
            // This should never happen, so we force an unwrap.
            .expect("failed to serialize initialization object"))
    });

    // We know the stream is unwind safe as it doesn't use synchronization primitives like locks.
    let answer_stream = AssertUnwindSafe(stream)
        .catch_unwind()
        .map(|res| res.unwrap_or_else(|_| Err(anyhow!("stream panicked"))))
        .map(|ex: Result<Exchange>| {
            sse::Event::default()
                .json_data(ex.map(Exchange::encode).map_err(|e| e.to_string()))
                .map_err(anyhow::Error::new)
        });

    let done_stream = futures::stream::once(async { Ok(sse::Event::default().data("[DONE]")) });

    let stream = init_stream.chain(answer_stream).chain(done_stream);

    Ok(Sse::new(Box::pin(stream)))
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodeChunk {
    path: String,
    #[serde(rename = "alias")]
    alias: usize,
    #[serde(rename = "snippet")]
    snippet: String,
    #[serde(rename = "start")]
    start_line: usize,
    #[serde(rename = "end")]
    end_line: usize,
}

impl CodeChunk {
    /// Returns true if a code-chunk contains an empty snippet or a snippet with only whitespace
    fn is_empty(&self) -> bool {
        self.snippet.trim().is_empty()
    }
}

enum AgentError {
    Timeout(Duration),
    Processing(anyhow::Error),
}

struct Agent {
    app: Application,
    repo_ref: RepoRef,
    exchanges: Vec<Exchange>,
    exchange_tx: Sender<Exchange>,

    llm_gateway: llm_gateway::Client,
    user: User,
    thread_id: uuid::Uuid,
    query_id: uuid::Uuid,

    /// Indicate whether the request was answered.
    ///
    /// This is used in the `Drop` handler, in order to track cancelled answer queries.
    complete: bool,
}

/// We use a `Drop` implementation to track agent query cancellation.
///
/// Query control flow can be complex, as there are several points where an error may be returned
/// via `?`. Rather than dealing with this in a complex way, we can simply use `Drop` destructors
/// to send cancellation messages to our analytics provider.
///
/// By default, dropping an agent struct will send a cancellation message. However, calling
/// `.complete()` will "diffuse" tracking, and disable the cancellation message from sending on drop.
impl Drop for Agent {
    fn drop(&mut self) {
        if !self.complete {
            self.track_query(
                EventData::output_stage("cancelled")
                    .with_payload("message", "request was cancelled"),
            );
        }
    }
}

impl Agent {
    /// Mark this agent as "completed", preventing an analytics message from sending on drop.
    fn complete(&mut self) {
        self.complete = true;
    }

    /// Update the last exchange
    async fn update(&mut self, update: Update) -> Result<()> {
        self.last_exchange_mut().apply_update(update);

        // Immutable reborrow of `self`
        let self_ = &*self;
        self_
            .exchange_tx
            .send(self.last_exchange().clone())
            .await
            .map_err(|_| anyhow!("exchange_tx was closed"))
    }

    fn track_query(&self, data: EventData) {
        let event = QueryEvent {
            query_id: self.query_id,
            thread_id: self.thread_id,
            repo_ref: Some(self.repo_ref.clone()),
            data,
        };
        self.app.track_query(&self.user, &event);
    }

    fn last_exchange(&self) -> &Exchange {
        self.exchanges.last().expect("exchange list was empty")
    }

    fn last_exchange_mut(&mut self) -> &mut Exchange {
        self.exchanges.last_mut().expect("exchange list was empty")
    }

    fn code_chunks(&self) -> impl Iterator<Item = CodeChunk> + '_ {
        self.exchanges
            .iter()
            .flat_map(|e| e.code_chunks.iter().cloned())
    }

    fn paths(&self) -> Vec<String> {
        self.exchanges
            .iter()
            .flat_map(|e| e.paths.iter().cloned())
            .collect::<Vec<_>>()
    }

    fn get_path_alias(&mut self, path: &str) -> usize {
        if let Some(i) = self.paths().iter().position(|p| *p == path) {
            i
        } else {
            let i = self.paths().len();
            self.last_exchange_mut().paths.push(path.to_owned());
            i
        }
    }

    async fn step(&mut self, action: Action) -> Result<Option<Action>> {
        debug!(?action, %self.thread_id, "executing next action");

        match &action {
            Action::Query(s) => {
                self.track_query(EventData::input_stage("query").with_payload("q", s));
                s.clone()
            }

            Action::Answer { paths } => {
                self.answer(paths).await?;
                return Ok(None);
            }

            Action::Path { query } => self.path_search(query).await?,
            Action::Code { query } => self.code_search(query).await?,
            Action::Proc { query, paths } => self.process_files(query, paths).await?,
        };

        let functions = serde_json::from_value::<Vec<llm_gateway::api::Function>>(
            prompts::functions(!self.paths().is_empty()), // Only add proc if there are paths in context
        )
        .unwrap();

        let mut history = vec![llm_gateway::api::Message::system(&prompts::system(
            &self.paths(),
        ))];
        history.extend(self.history()?);

        let trimmed_history = trim_history(history.clone())?;

        let raw_response = self
            .llm_gateway
            .chat(&trim_history(history.clone())?, Some(&functions))
            .await?
            .try_fold(
                llm_gateway::api::FunctionCall::default(),
                |acc, e| async move {
                    let e: FunctionCall = serde_json::from_str(&e)?;
                    Ok(FunctionCall {
                        name: acc.name.or(e.name),
                        arguments: acc.arguments + &e.arguments,
                    })
                },
            )
            .await?;

        self.track_query(
            EventData::output_stage("llm_reply")
                .with_payload("full_history", &history)
                .with_payload("trimmed_history", &trimmed_history)
                .with_payload("last_message", history.last())
                .with_payload("functions", &functions)
                .with_payload("raw_response", &raw_response),
        );

        let action = Action::deserialize_gpt(&raw_response)?;
        Ok(Some(action))
    }

    async fn code_search(&mut self, query: &String) -> Result<String> {
        const CODE_SEARCH_LIMIT: u64 = 10;
        self.update(Update::StartStep(SearchStep::Code {
            query: query.clone(),
            response: String::new(),
        }))
        .await?;

        let mut results = self
            .semantic_search(query.into(), CODE_SEARCH_LIMIT, 0, true)
            .await?;

        let hyde_docs = self.hyde(query).await?;
        if !hyde_docs.is_empty() {
            let hyde_doc = hyde_docs.first().unwrap().into();
            let hyde_results = self
                .semantic_search(hyde_doc, CODE_SEARCH_LIMIT, 0, true)
                .await?;
            results.extend(hyde_results);
        }

        let chunks = results
            .into_iter()
            .map(|chunk| {
                let relative_path = chunk.relative_path;

                CodeChunk {
                    path: relative_path.clone(),
                    alias: self.get_path_alias(&relative_path),
                    snippet: chunk.text,
                    start_line: (chunk.start_line as usize).saturating_add(1),
                    end_line: (chunk.end_line as usize).saturating_add(1),
                }
            })
            .collect::<Vec<_>>();

        for chunk in chunks.iter().filter(|c| !c.is_empty()) {
            self.exchanges
                .last_mut()
                .unwrap()
                .code_chunks
                .push(chunk.clone())
        }

        let response = serde_json::to_string(&chunks).unwrap();

        self.update(Update::ReplaceStep(SearchStep::Code {
            query: query.clone(),
            response: response.clone(),
        }))
        .await?;

        self.track_query(
            EventData::input_stage("semantic code search")
                .with_payload("query", query)
                .with_payload("hyde_queries", &hyde_docs)
                .with_payload("chunks", &chunks)
                .with_payload("raw_prompt", &response),
        );

        Ok(response)
    }

    async fn path_search(&mut self, query: &String) -> Result<String> {
        self.update(Update::StartStep(SearchStep::Path {
            query: query.clone(),
            response: String::new(),
        }))
        .await?;

        // First, perform a lexical search for the path
        let mut paths = self
            .fuzzy_path_search(query)
            .await
            .map(|c| c.relative_path)
            .collect::<HashSet<_>>() // TODO: This shouldn't be necessary. Path search should return unique results.
            .into_iter()
            .collect::<Vec<_>>();

        let is_semantic = paths.is_empty();

        // If there are no lexical results, perform a semantic search.
        if paths.is_empty() {
            let semantic_paths = self
                .semantic_search(query.into(), 30, 0, true)
                .await?
                .into_iter()
                .map(|chunk| chunk.relative_path)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            paths = semantic_paths;
        }

        let formatted_paths = paths
            .iter()
            .map(|p| (p.to_string(), self.get_path_alias(p)))
            .collect::<Vec<_>>();

        let response = serde_json::to_string(&formatted_paths).unwrap();

        self.update(Update::ReplaceStep(SearchStep::Path {
            query: query.clone(),
            response: response.clone(),
        }))
        .await?;

        self.track_query(
            EventData::input_stage("path search")
                .with_payload("query", query)
                .with_payload("is_semantic", is_semantic)
                .with_payload("results", &paths)
                .with_payload("raw_prompt", &response),
        );

        Ok(response)
    }

    async fn process_files(&mut self, query: &str, path_aliases: &[usize]) -> Result<String> {
        const MAX_CHUNK_LINE_LENGTH: usize = 20;
        const CHUNK_MERGE_DISTANCE: usize = 10;
        const MAX_TOKENS: usize = 15400;

        let paths = path_aliases
            .iter()
            .copied()
            .map(|i| self.paths().get(i).ok_or(i).cloned())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|i| anyhow!("invalid path alias {i}"))?;

        self.update(Update::StartStep(SearchStep::Proc {
            query: query.to_string(),
            paths: paths.clone(),
            response: String::new(),
        }))
        .await?;

        // Immutable reborrow of `self`, to copy freely to async closures.
        let self_ = &*self;
        let chunks = stream::iter(paths.clone())
            .map(|path| async move {
                tracing::debug!(?path, "reading file");

                let lines = self_
                    .get_file_content(&path)
                    .await?
                    .with_context(|| format!("path does not exist in the index: {path}"))?
                    .content
                    .lines()
                    .enumerate()
                    .map(|(i, line)| format!("{} {line}", i + 1))
                    .collect::<Vec<_>>();

                let bpe = tiktoken_rs::get_bpe_from_model("gpt-3.5-turbo")?;

                let iter =
                    tokio::task::spawn_blocking(|| trim_lines_by_tokens(lines, bpe, MAX_TOKENS))
                        .await
                        .context("failed to split by token")?;

                Result::<_>::Ok((iter, path.clone()))
            })
            // Buffer file loading to load multiple paths at once
            .buffered(10)
            .map(|result| async {
                let (lines, path) = result?;

                // The unwraps here should never fail, we generated this string above to always
                // have the same format.
                let start_line = lines[0]
                    .split_once(' ')
                    .unwrap()
                    .0
                    .parse::<usize>()
                    .unwrap();

                // We store the lines separately, so that we can reference them later to trim
                // this snippet by line number.
                let contents = lines.join("\n");
                let prompt = prompts::file_explanation(query, &path, &contents);

                debug!(?path, "calling chat API on file");

                let json = self_
                    .llm_gateway
                    .clone()
                    .model("gpt-3.5-turbo-16k-0613")
                    // Set low frequency penalty to discourage long outputs.
                    .frequency_penalty(0.1)
                    .chat(&[llm_gateway::api::Message::system(&prompt)], None)
                    .await?
                    .try_collect::<String>()
                    .await?;

                #[derive(
                    serde::Deserialize,
                    serde::Serialize,
                    PartialEq,
                    Eq,
                    PartialOrd,
                    Ord,
                    Copy,
                    Clone,
                    Debug,
                )]
                struct Range {
                    start: usize,
                    end: usize,
                }

                #[derive(serde::Serialize)]
                struct RelevantChunk {
                    #[serde(flatten)]
                    range: Range,
                    code: String,
                }

                impl RelevantChunk {
                    fn enumerate_lines(&self) -> Self {
                        Self {
                            range: self.range,
                            code: self
                                .code
                                .lines()
                                .enumerate()
                                .map(|(i, line)| format!("{} {line}", i + self.range.start))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        }
                    }
                }

                let mut line_ranges: Vec<Range> = serde_json::from_str::<Vec<Range>>(&json)?
                    .into_iter()
                    .filter(|r| r.start > 0 && r.end > 0)
                    .map(|mut r| {
                        r.end = r.end.min(r.start + MAX_CHUNK_LINE_LENGTH); // Cap relevant chunk size by line number
                        r
                    })
                    .collect();

                line_ranges.sort();
                line_ranges.dedup();

                let relevant_chunks = line_ranges
                    .into_iter()
                    .fold(Vec::<Range>::new(), |mut exps, next| {
                        if let Some(prev) = exps.last_mut() {
                            if prev.end + CHUNK_MERGE_DISTANCE >= next.start {
                                prev.end = next.end;
                                return exps;
                            }
                        }

                        exps.push(next);
                        exps
                    })
                    .into_iter()
                    .filter_map(|range| {
                        Some(RelevantChunk {
                            range,
                            code: lines
                                .get(
                                    range.start.saturating_sub(start_line)
                                        ..range.end.saturating_sub(start_line),
                                )?
                                .iter()
                                .map(|line| line.split_once(' ').unwrap().1)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        })
                    })
                    .collect::<Vec<_>>();

                Ok::<_, anyhow::Error>((relevant_chunks, path))
            });

        let processed = chunks
            // This box seems unnecessary, but it avoids a compiler bug:
            // https://github.com/rust-lang/rust/issues/64552
            .boxed()
            .buffered(5)
            .filter_map(|res| async { res.ok() })
            .collect::<Vec<_>>()
            .await;

        for (relevant_chunks, path) in &processed {
            let alias = self.get_path_alias(path);

            for c in relevant_chunks {
                let chunk = CodeChunk {
                    path: path.to_owned(),
                    alias,
                    snippet: c.code.clone(),
                    start_line: c.range.start,
                    end_line: c.range.end,
                };
                if !chunk.is_empty() {
                    self.last_exchange_mut().code_chunks.push(chunk);
                }
            }
        }

        let out = processed
            .into_iter()
            .map(|(relevant_chunks, path)| {
                serde_json::json!({
                    "relevant_chunks": relevant_chunks
                        .iter()
                        .map(|c| c.enumerate_lines())
                        .collect::<Vec<_>>(),
                    "path_alias": self.get_path_alias(&path),
                })
            })
            .collect::<Vec<_>>();

        let response = serde_json::to_string(&out)?;

        self.update(Update::ReplaceStep(SearchStep::Proc {
            query: query.to_string(),
            paths,
            response: response.clone(),
        }))
        .await?;

        self.track_query(
            EventData::input_stage("process file")
                .with_payload("question", query)
                .with_payload("chunks", &out)
                .with_payload("raw_prompt", &response),
        );

        Ok(response)
    }

    async fn answer_context(&mut self, aliases: &[usize], gpt_model: &str) -> Result<String> {
        let paths = self.paths();

        let mut s = "".to_owned();

        let mut aliases = aliases
            .iter()
            .copied()
            .filter(|alias| *alias < paths.len())
            .collect::<Vec<_>>();

        aliases.sort();
        aliases.dedup();

        debug!(?paths, ?aliases, "created filtered path alias list");

        // NB: If we have more than one selected alias passed to the agent `none` tool, we
        // intentionally ignore the alias list. This is part of a bigger issue that is to be
        // discussed and investigated separately, that points to odd behaviour with the agent
        // implementation.
        let aliases = if aliases.len() == 1 {
            aliases
        } else {
            (0..paths.len()).collect()
        };

        if !aliases.is_empty() {
            s += "##### PATHS #####\npath alias, path\n";

            for alias in &aliases {
                let path = &paths[*alias];
                s += &format!("{alias}, {path}\n");
            }
        }

        let code_chunks = self.canonicalize_code_chunks(&aliases, gpt_model).await;

        // Sometimes, there are just too many code chunks in the context, and deduplication still
        // doesn't trim enough chunks. So, we enforce a hard limit here that stops adding tokens
        // early if we reach a heuristic limit.
        const PROMPT_HEADROOM: usize = 2500;
        let bpe = tiktoken_rs::get_bpe_from_model(gpt_model)?;
        let mut remaining_prompt_tokens = tiktoken_rs::get_completion_max_tokens(gpt_model, &s)?;

        // Select as many recent chunks as possible
        let mut recent_chunks = Vec::new();
        for chunk in code_chunks.iter().rev() {
            let snippet = chunk
                .snippet
                .lines()
                .enumerate()
                .map(|(i, line)| format!("{} {line}\n", i + chunk.start_line))
                .collect::<String>();

            let formatted_snippet = format!("### path alias: {} ###\n{snippet}\n\n", chunk.alias);

            let snippet_tokens = bpe.encode_ordinary(&formatted_snippet).len();

            if snippet_tokens >= remaining_prompt_tokens - PROMPT_HEADROOM {
                debug!("Breaking at {} tokens...", remaining_prompt_tokens);
                break;
            }

            recent_chunks.push((chunk.clone(), formatted_snippet));

            remaining_prompt_tokens -= snippet_tokens;
            debug!("{}", remaining_prompt_tokens);
        }

        // group recent chunks by path alias
        let mut recent_chunks_by_alias: HashMap<_, _> =
            recent_chunks
                .into_iter()
                .fold(HashMap::new(), |mut map, item| {
                    map.entry(item.0.alias).or_insert_with(Vec::new).push(item);
                    map
                });

        // write the header if we have atleast one chunk
        if !recent_chunks_by_alias.values().all(Vec::is_empty) {
            s += "\n##### CODE CHUNKS #####\n\n";
        }

        // sort by alias, then sort by lines
        let mut aliases = recent_chunks_by_alias.keys().copied().collect::<Vec<_>>();
        aliases.sort();

        for alias in aliases {
            let chunks = recent_chunks_by_alias.get_mut(&alias).unwrap();
            chunks.sort_by(|a, b| a.0.start_line.cmp(&b.0.start_line));
            for (_, formatted_snippet) in chunks {
                s += formatted_snippet;
            }
        }

        Ok(s)
    }

    async fn answer(&mut self, aliases: &[usize]) -> Result<()> {
        const ANSWER_ARTICLE_MODEL: &str = "gpt-4-0613";

        debug!(?aliases, "creating article response");

        let context = self.answer_context(aliases, ANSWER_ARTICLE_MODEL).await?;
        let history = self.utter_history().collect::<Vec<_>>();

        let system_message = prompts::answer_article_prompt(&context);
        let messages = Some(llm_gateway::api::Message::system(&system_message))
            .into_iter()
            .chain(history.iter().cloned())
            .collect::<Vec<_>>();

        let mut stream = pin!(
            self.llm_gateway
                .clone()
                .model(ANSWER_ARTICLE_MODEL)
                .chat(&messages, None)
                .await?
        );

        let mut response = String::new();
        while let Some(fragment) = stream.next().await {
            let fragment = fragment?;
            response += &fragment;

            if let Some((article, summary)) = split_article_summary(&response) {
                self.update(Update::Article(article)).await?;
                self.update(Update::Conclude(summary)).await?;
            } else {
                self.update(Update::Article(response.clone())).await?;
            }
        }

        let summary = split_article_summary(&response)
            .map(|(_article, summary)| summary)
            .unwrap_or_else(|| {
                [
                    "I hope that was useful, can I help with anything else?",
                    "Is there anything else I can help you with?",
                    "Can I help you with anything else?",
                ]
                .choose(&mut OsRng)
                .copied()
                .unwrap()
                .to_owned()
            });

        self.update(Update::Conclude(summary)).await?;

        self.track_query(
            EventData::output_stage("answer_article")
                .with_payload("query", self.last_exchange().query())
                .with_payload("query_history", &history)
                .with_payload("response", &response)
                .with_payload("raw_prompt", &system_message),
        );

        Ok(())
    }

    /// The full history of messages, including intermediate function calls
    fn history(&self) -> Result<Vec<llm_gateway::api::Message>> {
        let history = self
            .exchanges
            .iter()
            .try_fold(Vec::new(), |mut acc, e| -> Result<_> {
                let query = e
                    .query()
                    .map(|q| {
                        llm_gateway::api::Message::user(&format!(
                            "{q}\nCall a function. Do not answer."
                        ))
                    })
                    .ok_or_else(|| anyhow!("query does not have target"))?;

                let steps = e.search_steps.iter().flat_map(|s| {
                    let (name, arguments) = match s {
                        SearchStep::Path { query, .. } => (
                            "path".to_owned(),
                            format!("{{\n \"query\": \"{query}\"\n}}"),
                        ),
                        SearchStep::Code { query, .. } => (
                            "code".to_owned(),
                            format!("{{\n \"query\": \"{query}\"\n}}"),
                        ),
                        SearchStep::Proc { query, paths, .. } => (
                            "proc".to_owned(),
                            format!(
                                "{{\n \"paths\": [{}],\n \"query\": \"{query}\"\n}}",
                                paths
                                    .iter()
                                    .map(|path| self
                                        .paths()
                                        .iter()
                                        .position(|p| p == path)
                                        .unwrap()
                                        .to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        ),
                    };

                    vec![
                        llm_gateway::api::Message::function_call(&FunctionCall {
                            name: Some(name.clone()),
                            arguments,
                        }),
                        llm_gateway::api::Message::function_return(
                            &name,
                            &format!("{}\nCall a function. Do not answer.", s.get_response()),
                        ),
                    ]
                });

                let answer = e
                    .answer_summarized()?
                    .map(|a| llm_gateway::api::Message::assistant(&a));

                acc.extend(
                    std::iter::once(query)
                        .chain(steps)
                        .chain(answer.into_iter()),
                );
                Ok(acc)
            })?;
        Ok(history)
    }

    /// History of `user`, `assistant` messages. These are the messages that are shown to the user.
    fn utter_history(&self) -> impl Iterator<Item = llm_gateway::api::Message> + '_ {
        const ANSWER_MAX_HISTORY_SIZE: usize = 5;

        self.exchanges
            .iter()
            .rev()
            .take(ANSWER_MAX_HISTORY_SIZE)
            .rev()
            .flat_map(|e| {
                let query = e.query().map(|q| llm_gateway::api::Message::PlainText {
                    role: "user".to_owned(),
                    content: q,
                });

                let conclusion = e.answer().map(|c| llm_gateway::api::Message::PlainText {
                    role: "assistant".to_owned(),
                    content: c.to_owned(),
                });

                query
                    .into_iter()
                    .chain(conclusion.into_iter())
                    .collect::<Vec<_>>()
            })
    }

    /// Merge overlapping and nearby code chunks
    async fn canonicalize_code_chunks(
        &mut self,
        aliases: &[usize],
        gpt_model: &str,
    ) -> Vec<CodeChunk> {
        debug!(?aliases, "canonicalizing code chunks");

        /// The ratio of code tokens to context size.
        ///
        /// Making this closure to 1 means that more of the context is taken up by source code.
        const CONTEXT_CODE_RATIO: f32 = 0.5;

        let bpe = tiktoken_rs::get_bpe_from_model(gpt_model).unwrap();
        let context_size = tiktoken_rs::model::get_context_size(gpt_model);
        let max_tokens = (context_size as f32 * CONTEXT_CODE_RATIO) as usize;

        let mut spans_by_path = HashMap::<_, Vec<_>>::new();
        for c in self.code_chunks().filter(|c| aliases.contains(&c.alias)) {
            spans_by_path
                .entry(c.path.clone())
                .or_default()
                .push(c.start_line..c.end_line);
        }

        debug!(?spans_by_path, "expanding spans");

        let self_ = &*self;
        // Map of path -> line list
        let lines_by_file = futures::stream::iter(&mut spans_by_path)
            .then(|(path, spans)| async move {
                spans.sort_by_key(|c| c.start);

                let lines = self_
                    .get_file_content(path)
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("path did not exist in the index: {path}"))
                    .content
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();

                (path.clone(), lines)
            })
            .collect::<HashMap<_, _>>()
            .await;

        // Total number of lines to try and expand by, per loop iteration.
        const TOTAL_LINE_INC: usize = 100;

        // We keep track of whether any spans were changed below, so that we know when to break
        // out of this loop.
        let mut changed = true;

        while !spans_by_path.is_empty() && changed {
            changed = false;

            let tokens = spans_by_path
                .iter()
                .flat_map(|(path, spans)| spans.iter().map(move |s| (path, s)))
                .map(|(path, span)| {
                    let range = span.start.saturating_sub(1)..span.end.saturating_sub(1);
                    let snippet = lines_by_file.get(path).unwrap()[range].join("\n");
                    bpe.encode_ordinary(&snippet).len()
                })
                .sum::<usize>();

            // First, we grow the spans if possible.
            if tokens < max_tokens {
                // NB: We divide TOTAL_LINE_INC by 2, because we expand in 2 directions.
                let range_step = (TOTAL_LINE_INC / 2)
                    / spans_by_path
                        .values()
                        .map(|spans| spans.len())
                        .sum::<usize>()
                        .max(1);

                let range_step = range_step.max(1);

                for (path, span) in spans_by_path
                    .iter_mut()
                    .flat_map(|(path, spans)| spans.iter_mut().map(move |s| (path, s)))
                {
                    let file_lines = lines_by_file.get(path.as_str()).unwrap().len();

                    let old_span = span.clone();

                    // Decrease the start line, but make sure that we don't end up with 0, as our lines
                    // are 1-based.
                    span.start = span.start.saturating_sub(range_step).max(1);

                    // Expand the end line forwards, capping at the total number of lines (NB: this is
                    // also 1-based).
                    span.end += range_step;
                    span.end = span.end.min(file_lines);

                    if *span != old_span {
                        debug!(?path, "growing span");
                        changed = true;
                    }
                }
            }

            // Next, we merge any overlapping spans.
            for spans in spans_by_path.values_mut() {
                *spans = mem::take(spans)
                    .into_iter()
                    .fold(Vec::new(), |mut a, next| {
                        // There is some rightward drift here, which could be fixed once if-let
                        // chains are stabilized.
                        if let Some(prev) = a.last_mut() {
                            if let Some(next) = merge_overlapping(prev, next) {
                                a.push(next);
                            } else {
                                changed = true;
                            }
                        } else {
                            a.push(next);
                        }

                        a
                    });
            }
        }

        debug!(?spans_by_path, "expanded spans");

        spans_by_path
            .into_iter()
            .flat_map(|(path, spans)| spans.into_iter().map(move |s| (path.clone(), s)))
            .map(|(path, span)| {
                let range = span.start.saturating_sub(1)..span.end.saturating_sub(1);
                let snippet = lines_by_file.get(&path).unwrap()[range].join("\n");

                CodeChunk {
                    alias: self.get_path_alias(&path),
                    path,
                    snippet,
                    start_line: span.start,
                    end_line: span.end,
                }
            })
            .collect()
    }

    /// Hypothetical Document Embedding (HyDE): https://arxiv.org/abs/2212.10496
    ///
    /// This method generates synthetic documents based on the query. These are then
    /// parsed and code is extracted. This has been shown to improve semantic search recall.
    async fn hyde(&self, query: &str) -> Result<Vec<String>> {
        let prompt = vec![llm_gateway::api::Message::system(
            &prompts::hypothetical_document_prompt(query),
        )];

        tracing::trace!(?query, "generating hyde docs");

        let response = self
            .llm_gateway
            .clone()
            .model("gpt-3.5-turbo-0613")
            .chat(&prompt, None)
            .await?
            .try_collect::<String>()
            .await?;

        tracing::trace!("parsing hyde response");

        let documents = prompts::try_parse_hypothetical_documents(&response);

        for doc in documents.iter() {
            info!(?doc, "got hyde doc");
        }

        Ok(documents)
    }

    async fn semantic_search(
        &self,
        query: Literal<'_>,
        limit: u64,
        offset: u64,
        retrieve_more: bool,
    ) -> Result<Vec<semantic::Payload>> {
        let query = SemanticQuery {
            target: Some(query),
            repos: [Literal::Plain(self.repo_ref.display_name().into())].into(),
            ..self.last_exchange().query.clone()
        };

        debug!(?query, %self.thread_id, "executing semantic query");
        self.app
            .semantic
            .as_ref()
            .unwrap()
            .search(&query, limit, offset, retrieve_more)
            .await
    }

    #[allow(dead_code)]
    async fn batch_semantic_search(
        &self,
        queries: Vec<Literal<'_>>,
        limit: u64,
        offset: u64,
        retrieve_more: bool,
    ) -> Result<Vec<semantic::Payload>> {
        let queries = queries
            .iter()
            .map(|q| SemanticQuery {
                target: Some(q.clone()),
                repos: [Literal::Plain(self.repo_ref.display_name().into())].into(),
                ..self.last_exchange().query.clone()
            })
            .collect::<Vec<_>>();

        let queries = queries.iter().collect::<Vec<_>>();

        debug!(?queries, %self.thread_id, "executing semantic query");
        self.app
            .semantic
            .as_ref()
            .unwrap()
            .batch_search(queries.as_slice(), limit, offset, retrieve_more)
            .await
    }

    async fn get_file_content(&self, path: &str) -> Result<Option<ContentDocument>> {
        let branch = self.last_exchange().query.first_branch();

        debug!(%self.repo_ref, path, ?branch, %self.thread_id, "executing file search");
        self.app
            .indexes
            .file
            .by_path(&self.repo_ref, path, branch.as_deref())
            .await
            .with_context(|| format!("failed to read path: {}", path))
    }

    async fn fuzzy_path_search<'a>(
        &'a self,
        query: &str,
    ) -> impl Iterator<Item = FileDocument> + 'a {
        let branch = self.last_exchange().query.first_branch();

        debug!(%self.repo_ref, query, ?branch, %self.thread_id, "executing fuzzy search");
        self.app
            .indexes
            .file
            .fuzzy_path_match(&self.repo_ref, query, branch.as_deref(), 50)
            .await
    }
}

fn trim_history(
    mut history: Vec<llm_gateway::api::Message>,
) -> Result<Vec<llm_gateway::api::Message>> {
    const HEADROOM: usize = 2048;

    let mut tiktoken_msgs = history
        .iter()
        .map(|m| match m {
            llm_gateway::api::Message::PlainText { role, content } => {
                tiktoken_rs::ChatCompletionRequestMessage {
                    role: role.clone(),
                    content: content.clone(),
                    name: None,
                }
            }
            llm_gateway::api::Message::FunctionReturn {
                role,
                name,
                content,
            } => tiktoken_rs::ChatCompletionRequestMessage {
                role: role.clone(),
                content: content.clone(),
                name: Some(name.clone()),
            },
            llm_gateway::api::Message::FunctionCall {
                role,
                function_call,
                content: _,
            } => tiktoken_rs::ChatCompletionRequestMessage {
                role: role.clone(),
                content: serde_json::to_string(&function_call).unwrap(),
                name: None,
            },
        })
        .collect::<Vec<_>>();

    while tiktoken_rs::get_chat_completion_max_tokens("gpt-4", &tiktoken_msgs)? < HEADROOM {
        let idx = history
            .iter_mut()
            .position(|m| match m {
                llm_gateway::api::Message::PlainText {
                    role,
                    ref mut content,
                } if (role == "user" || role == "assistant") && content != "[HIDDEN]" => {
                    *content = "[HIDDEN]".into();
                    true
                }
                llm_gateway::api::Message::FunctionReturn {
                    role: _,
                    name: _,
                    ref mut content,
                } if content != "[HIDDEN]" => {
                    *content = "[HIDDEN]".into();
                    true
                }
                _ => false,
            })
            .ok_or_else(|| anyhow!("could not find message to trim"))?;

        tiktoken_msgs[idx].content = "[HIDDEN]".into();
    }

    Ok(history)
}

fn trim_lines_by_tokens(lines: Vec<String>, bpe: CoreBPE, max_tokens: usize) -> Vec<String> {
    let line_tokens = lines
        .iter()
        .map(|line| bpe.encode_ordinary(line).len())
        .collect::<Vec<_>>();

    let mut trimmed_lines = Vec::new();

    // Push lines to `trimmed_lines` until we reach the maximum number of tokens.
    let mut i = 0usize;
    let mut tokens = 0usize;
    while i < lines.len() && tokens < max_tokens {
        tokens += line_tokens[i];
        trimmed_lines.push(lines[i].clone());
        i += 1;
    }

    trimmed_lines
}

fn limit_tokens(text: &str, bpe: CoreBPE, max_tokens: usize) -> &str {
    let mut tokens = bpe.encode_ordinary(text);
    tokens.truncate(max_tokens);

    while !tokens.is_empty() {
        if let Ok(s) = bpe.decode(tokens.clone()) {
            return &text[..s.len()];
        }

        let _ = tokens.pop();
    }

    ""
}

/// Merge line ranges if they overlap.
///
/// This function assumes that the first parameter is a line range which starts *before* the line
/// range given by the second parameter.
fn merge_overlapping(a: &mut Range<usize>, b: Range<usize>) -> Option<Range<usize>> {
    if a.end >= b.start {
        // `b` might be contained in `a`, which allows us to discard it.
        if a.end < b.end {
            a.end = b.end;
        }

        None
    } else {
        Some(b)
    }
}

fn split_article_summary(response: &str) -> Option<(String, String)> {
    // The `comrak` crate has a very unusual API which makes this logic difficult to follow. It
    // favours arena allocation instead of a tree-based AST, and requires `Write`rs to regenerate
    // markdown output.
    //
    // There are quirks to the parsing logic, comments have been added for clarity.

    let arena = comrak::Arena::new();
    let mut options = comrak::ComrakOptions::default();
    options.extension.footnotes = true;

    // We don't have an easy built-in way to generate a string with `comrak`, so we encapsulate
    // that logic here.
    let comrak_to_string = |node| {
        let mut out = Vec::<u8>::new();
        comrak::format_commonmark(node, &options, &mut out).unwrap();
        String::from_utf8_lossy(&out).trim().to_owned()
    };

    // `comrak` will not recognize footnote definitions unless they have been referenced at least
    // once. To ensure our potential summary appears in the parse tree, we prepend the entire
    // response with a sentinel reference to the footnote. After parsing, we look for that
    // footnote and immediately remove (detach) it from the root node. This ensures that our
    // artifical reference does not appear in the output.

    let document = format!("[^summary]\n\n{response}");
    let root = comrak::parse_document(&arena, &document, &options);
    let mut children = root.children();
    // Detach the sentinel footnote reference.
    children.next().unwrap().detach();

    for child in children {
        match &child.data.borrow().value {
            comrak::nodes::NodeValue::FootnoteDefinition(def) if def.name == "summary" => (),
            _ => continue,
        };

        let first_child = child.children().next()?;
        if let comrak::nodes::NodeValue::Paragraph = &first_child.data.borrow().value {
            // We detach the summary from the main text, so that it does not end up in the final
            // article output.
            child.detach();
            return Some((comrak_to_string(root), comrak_to_string(first_child)));
        }
    }

    None
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    /// A user-provided query.
    Query(String),

    Path {
        query: String,
    },
    #[serde(rename = "none")]
    Answer {
        paths: Vec<usize>,
    },
    Code {
        query: String,
    },
    Proc {
        query: String,
        paths: Vec<usize>,
    },
}

impl Action {
    /// Deserialize this action from the GPT-tagged enum variant format.
    ///
    /// We convert (2 examples):
    ///
    /// ```text
    /// {"name": "Variant1", "args": {}}
    /// {"name": "Variant2", "args": {"a":123}}
    /// ```
    ///
    /// To:
    ///
    /// ```text
    /// {"Variant1": {}}
    /// {"Variant2": {"a":123}}
    /// ```
    ///
    /// So that we can deserialize using the serde-provided "tagged" enum representation.
    fn deserialize_gpt(call: &FunctionCall) -> Result<Self> {
        let mut map = serde_json::Map::new();
        map.insert(
            call.name.clone().unwrap(),
            serde_json::from_str(&call.arguments)?,
        );

        Ok(serde_json::from_value(serde_json::Value::Object(map))?)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_trimming() {
        let long_string = "long string ".repeat(2000);
        let history = vec![
            llm_gateway::api::Message::system("foo"),
            llm_gateway::api::Message::user("bar"),
            llm_gateway::api::Message::assistant("baz"),
            llm_gateway::api::Message::user(&long_string),
            llm_gateway::api::Message::assistant("quux"),
            llm_gateway::api::Message::user("fred"),
            llm_gateway::api::Message::assistant("thud"),
            llm_gateway::api::Message::user(&long_string),
            llm_gateway::api::Message::user("corge"),
        ];

        assert_eq!(
            trim_history(history).unwrap(),
            vec![
                llm_gateway::api::Message::system("foo"),
                llm_gateway::api::Message::user("[HIDDEN]"),
                llm_gateway::api::Message::assistant("[HIDDEN]"),
                llm_gateway::api::Message::user("[HIDDEN]"),
                llm_gateway::api::Message::assistant("quux"),
                llm_gateway::api::Message::user("fred"),
                llm_gateway::api::Message::assistant("thud"),
                llm_gateway::api::Message::user(&long_string),
                llm_gateway::api::Message::user("corge"),
            ]
        );
    }

    #[test]
    fn test_trim_lines_by_tokens() {
        let bpe = tiktoken_rs::get_bpe_from_model("gpt-3.5-turbo").unwrap();

        let lines = vec![
            "fn main() {".to_string(),
            "    one();".to_string(),
            "    two();".to_string(),
            "    three();".to_string(),
            "    four();".to_string(),
            "    five();".to_string(),
            "    six();".to_string(),
            "}".to_string(),
        ];
        assert_eq!(
            trim_lines_by_tokens(lines, bpe.clone(), 15),
            vec![
                "fn main() {".to_string(),
                "    one();".to_string(),
                "    two();".to_string(),
                "    three();".to_string(),
                "    four();".to_string()
            ]
        );

        let lines = vec!["fn main() {".to_string(), "    one();".to_string()];
        assert_eq!(
            trim_lines_by_tokens(lines, bpe.clone(), 15),
            vec!["fn main() {".to_string(), "    one();".to_string()]
        );

        let expected: Vec<String> = vec![];
        assert_eq!(trim_lines_by_tokens(vec![], bpe, 15), expected);
    }

    #[test]
    fn test_limit_tokens() {
        let bpe = tiktoken_rs::get_bpe_from_model("gpt-3.5-turbo").unwrap();
        assert_eq!(limit_tokens("fn 🚨() {}", bpe.clone(), 1), "fn");

        // Note: the following calls return a string that does not split the emoji, despite the
        // tokenizer interpreting the tokens like that.
        assert_eq!(limit_tokens("fn 🚨() {}", bpe.clone(), 2), "fn");
        assert_eq!(limit_tokens("fn 🚨() {}", bpe.clone(), 3), "fn");

        // Now we have a sufficient number of input tokens to overcome the emoji.
        assert_eq!(limit_tokens("fn 🚨() {}", bpe.clone(), 4), "fn 🚨");
        assert_eq!(limit_tokens("fn 🚨() {}", bpe.clone(), 5), "fn 🚨()");
        assert_eq!(limit_tokens("fn 🚨() {}", bpe, 6), "fn 🚨() {}");
    }

    #[test]
    fn test_split_article_summary() {
        let (body, summary) = split_article_summary(
            r#"Hello world

[^summary]: This is an example summary, with **bold text**."#,
        )
        .unwrap();

        assert_eq!(body, "Hello world");
        assert_eq!(summary, "This is an example summary, with **bold text**.");

        let (body, summary) = split_article_summary(
            r#"Hello world.

Goodbye world.

Hello again, world.

[^summary]: This is an example summary, with **bold text**."#,
        )
        .unwrap();

        assert_eq!(
            body,
            "Hello world.\n\nGoodbye world.\n\nHello again, world."
        );
        assert_eq!(summary, "This is an example summary, with **bold text**.");
    }
}
