// Copyright 2020-2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::env;
use crate::errors::{Error, Result};
use crate::util::{get_source_kind, highlight, slurp_string, SourceKind};
use async_std::task::block_on;
use beef::Cow;
use clap::ArgMatches;
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter, Read, Write};
use tremor_common::{file, ids::OperatorIdGen, time::nanotime};
use tremor_pipeline::{Event, EventId};
use tremor_runtime::{
    codec::Codec,
    postprocessor::Postprocessor,
    preprocessor::Preprocessor,
    system::{ShutdownMode, World, WorldConfig},
};
use tremor_script::{
    ast::Helper,
    ctx::EventContext,
    highlighter::{Error as HighlighterError, Highlighter, Term as TermHighlighter},
    lexer::Tokenizer,
    prelude::*,
    query::Query,
    script::{AggrType, Return, Script},
    EventPayload, Value, ValueAndMeta,
};
use tremor_value::literal;

struct Ingress {
    is_interactive: bool,
    is_pretty: bool,
    buf: [u8; 4096],
    buffer: Box<dyn BufRead>,
    preprocessor: Box<dyn Preprocessor>,
    codec: Box<dyn Codec>,
}

type IngressHandler<T> =
    dyn Fn(&mut T, &mut u64, &mut Egress, &mut Value<'static>, u64, Value) -> Result<()>;

impl Ingress {
    fn from_args(matches: &ArgMatches) -> Result<Self> {
        let codec_pre = matches.value_of("PREPROCESSOR").unwrap_or("lines");
        let codec_decoder = matches.value_of("DECODER").unwrap_or("json");
        let is_interactive = matches.is_present("interactive");
        let is_pretty = matches.is_present("pretty");

        let buffer: Box<dyn BufRead> = match matches.value_of("INFILE") {
            None | Some("-") => Box::new(BufReader::new(io::stdin())),
            Some(data) => Box::new(BufReader::new(crate::open_file(data, None)?)),
        };

        let codec = tremor_runtime::codec::lookup(codec_decoder);
        if let Err(_e) = codec {
            eprintln!("Error Codec {} not found error.", codec_decoder);
            // ALLOW: main.rs
            std::process::exit(1);
        }
        let codec = codec?;
        let preprocessor = tremor_runtime::preprocessor::lookup(codec_pre);
        if let Err(_e) = preprocessor {
            eprintln!("Error Preprocessor {} not found error.", codec_pre);
            // ALLOW: main.rs
            std::process::exit(1);
        }
        let preprocessor = preprocessor?;

        Ok(Self {
            is_interactive,
            is_pretty,
            buf: [0_u8; 4096],
            preprocessor,
            codec,
            buffer,
        })
    }

    fn process<T>(
        &mut self,
        runnable: &mut T,
        mut id: u64,
        mut egress: &mut Egress,
        handler: &IngressHandler<T>,
    ) -> Result<()> {
        let mut state: Value<'static> = Value::null();
        loop {
            match self.buffer.read(&mut self.buf) {
                Ok(0) => {
                    // ALLOW: main.rs
                    return Ok(());
                }
                Ok(n) => {
                    let mut at = nanotime();
                    // We access the entire read buffer the len is provided by read
                    let x = self
                        .preprocessor
                        .process(&mut at, unsafe { self.buf.get_unchecked(0..n) })?;
                    for mut data in x {
                        let event = match self.codec.decode(data.as_mut_slice(), at) {
                            Ok(Some(data)) => data,
                            Ok(None) => continue,
                            Err(e) => return Err(e.into()),
                        };

                        if self.is_interactive {
                            eprintln!(
                                "ingress> [codec: {}], [preprocessor: {}]",
                                self.codec.name(),
                                self.preprocessor.name()
                            );
                            highlight(self.is_pretty, &event)?;
                        }
                        handler(runnable, &mut id, &mut egress, &mut state, at, event)?;
                    }
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    }
}

struct Egress {
    is_interactive: bool,
    is_pretty: bool,
    buffer: Box<dyn Write>,
    codec: Box<dyn Codec>,
    postprocessor: Box<dyn Postprocessor>,
}

impl Egress {
    fn from_args(matches: &ArgMatches) -> Result<Self> {
        let codec_post = matches.value_of("POSTPROCESSOR").unwrap_or("lines");
        let codec_encoder = matches.value_of("ENCODER").unwrap_or("json");
        let is_interactive = matches.is_present("interactive");
        let is_pretty = matches.is_present("pretty");

        let buffer: Box<dyn Write> = match matches.value_of("OUTFILE") {
            None | Some("-") => Box::new(BufWriter::new(io::stdout())),
            Some(data) => Box::new(BufWriter::new(file::create(data)?)),
        };

        let codec = tremor_runtime::codec::lookup(codec_encoder);
        if let Err(_e) = codec {
            eprintln!("Error Codec {} not found error.", codec_encoder);
            // ALLOW: main.rs
            std::process::exit(1);
        }
        let codec = codec?;

        let postprocessor = tremor_runtime::postprocessor::lookup(codec_post);
        if let Err(_e) = postprocessor {
            eprintln!("Error Postprocessor {} not found error.", codec_post);
            // ALLOW: main.rs
            std::process::exit(1);
        }
        let postprocessor = postprocessor?;

        Ok(Self {
            is_interactive,
            is_pretty,
            buffer,
            codec,
            postprocessor,
        })
    }

    fn process(&mut self, _src: &str, event: &Value, ret: Return) -> Result<()> {
        match ret {
            Return::Drop => Ok(()),
            Return::Emit { value, port } => {
                match port.unwrap_or_else(|| String::from("out")).as_str() {
                    "err" | "error" | "stderr" => {
                        self.buffer
                            .write_all(format!("{}\n", value.encode()).as_bytes())?;
                        self.buffer.flush()?;
                    }
                    _ => {
                        if self.is_interactive {
                            eprintln!(
                                "egress> [codec: {}], [postprocessor: {}]",
                                self.codec.name(),
                                self.postprocessor.name()
                            );
                            highlight(self.is_pretty, &value)?;
                        }

                        let encoded = self.codec.encode(&value);

                        let ppd = self
                            .postprocessor
                            .process(nanotime(), nanotime(), &encoded?);
                        for packet in ppd? {
                            self.buffer.write_all(&packet)?;
                            self.buffer.flush()?;
                        }
                    }
                };
                self.buffer.flush()?;
                Ok(())
            }
            Return::EmitEvent { port } => {
                match port.unwrap_or_else(|| String::from("out")).as_str() {
                    "err" | "error" | "stderr" => {
                        eprintln!("{}", event.encode());
                    }
                    _ => {
                        self.buffer
                            .write_all(format!("{}\n", event.encode()).as_bytes())?;
                        self.buffer.flush()?;
                    }
                };
                Ok(())
            }
        }
    }
}

fn run_tremor_source(matches: &ArgMatches, src: String, _args: &Value) -> Result<()> {
    let raw = slurp_string(&src);
    if let Err(e) = raw {
        eprintln!("Error processing file {}: {}", &src, e);
        // ALLOW: main.rs
        std::process::exit(1);
    }
    let raw = raw?;

    let env = env::setup()?;

    let mut outer = TermHighlighter::stderr();
    match Script::parse(&env.module_path, &src, raw.clone(), &env.fun) {
        Ok(mut script) => {
            script.format_warnings_with(&mut outer)?;

            let mut ingress = Ingress::from_args(matches)?;
            let mut egress = Egress::from_args(matches)?;
            let id = 0_u64;

            ingress.process(
                &mut script,
                id,
                &mut egress,
                &move |runnable, _id, egress, state, at, event| {
                    let mut global_map = Value::object();
                    let mut event = event.clone_static();
                    match runnable.run(
                        &EventContext::new(at, None),
                        AggrType::Tick,
                        &mut event,
                        state,
                        &mut global_map,
                    ) {
                        Ok(r) => egress.process(&src, &event, r),
                        Err(e) => {
                            if let (Some(r), _) = e.context() {
                                let mut inner = TermHighlighter::stderr();
                                let mut input = raw.clone();
                                input.push('\n'); // for nicer highlighting
                                let tokens: Vec<_> =
                                    Tokenizer::new(&input).tokenize_until_err().collect();

                                if let Err(highlight_error) = inner.highlight_error(
                                    Some(&src),
                                    &tokens,
                                    "",
                                    true,
                                    Some(r),
                                    Some(HighlighterError::from(&e)),
                                ) {
                                    eprintln!(
                                        "Error during error highlighting: {}",
                                        highlight_error
                                    );
                                    Err(highlight_error.into())
                                } else {
                                    inner.finalize()?;
                                    Ok(()) // error has already been displayed
                                }
                            } else {
                                eprintln!("Error processing event: {}", e);
                                Err(e.into())
                            }
                        }
                    }
                },
            )?;

            Ok(())
        }
        Err(e) => {
            if let Err(e) = Script::format_error_from_script(&raw, &mut outer, &e) {
                eprintln!("Error: {}", e);
            };

            // ALLOW: main.rs
            std::process::exit(1);
        }
    }
}

fn run_trickle_source(matches: &ArgMatches, src: &str, _args: &Value) -> Result<()> {
    let raw = slurp_string(src);
    if let Err(e) = raw {
        eprintln!("Error processing file {}: {}", src, e);
        // ALLOW: main.rs
        std::process::exit(1);
    }
    let raw = raw?;
    let env = env::setup()?;
    let mut h = TermHighlighter::stderr();

    let runnable = match Query::parse(&env.module_path, src, &raw, vec![], &env.fun, &env.aggr) {
        Ok(runnable) => runnable,
        Err(e) => {
            if let Err(e) = Script::format_error_from_script(&raw, &mut h, &e) {
                eprintln!("Error: {}", e);
            };
            // ALLOW: main.rs
            std::process::exit(1);
        }
    };
    run_trickle_query(matches, runnable, src.to_string(), raw, &mut h)
}

fn run_trickle_query(
    matches: &ArgMatches,
    runnable: Query,
    src: String,
    raw: String,
    h: &mut TermHighlighter,
) -> Result<()> {
    runnable.format_warnings_with(h)?;

    let mut ingress = Ingress::from_args(matches)?;
    let mut egress = Egress::from_args(matches)?;

    let runnable = tremor_pipeline::query::Query(runnable);
    let mut idgen = OperatorIdGen::new();
    let mut pipeline = runnable.to_pipe(&mut idgen)?;
    let id = 0_u64;

    ingress.process(
        &mut pipeline,
        id,
        &mut egress,
        &move |runnable, id, egress, _state, at, event| {
            let value = EventPayload::new(vec![], |_| ValueAndMeta::from(event.clone_static()));

            let mut continuation = vec![];

            if let Err(e) = runnable.enqueue(
                "in",
                Event {
                    id: EventId::from_id(0, 0, *id),
                    data: value.clone(),
                    ingest_ns: at,
                    ..Event::default()
                },
                &mut continuation,
            ) {
                match e.0 {
                    tremor_pipeline::errors::ErrorKind::Script(script_kind) => {
                        let script_error: tremor_script::errors::Error = script_kind.into();
                        if let (Some(r), _) = script_error.context() {
                            let mut inner = TermHighlighter::stderr();
                            let mut input = raw.clone();
                            input.push('\n'); // for nicer highlighting
                            let tokens: Vec<_> =
                                Tokenizer::new(&input).tokenize_until_err().collect();

                            if let Err(highlight_error) = inner.highlight_error(
                                Some(&src),
                                &tokens,
                                "",
                                true,
                                Some(r),
                                Some(HighlighterError::from(&script_error)),
                            ) {
                                eprintln!("Error during error highlighting: {}", highlight_error);
                                return Err(highlight_error.into());
                            }
                            inner.finalize()?;
                            return Ok(());
                        }
                    }
                    _ => {
                        return Err(e.into());
                    }
                }
            }
            *id += 1;

            for (port, rvalue) in continuation.drain(..) {
                egress.process(
                    &simd_json::to_string_pretty(&value.suffix().value())?,
                    &event,
                    Return::Emit {
                        value: rvalue.data.suffix().value().clone_static(),
                        port: Some(port.to_string()),
                    },
                )?;
            }

            Ok(())
        },
    )?;

    h.finalize()?;

    Ok(())
}

fn run_troy_source(_matches: &ArgMatches, file_name: &str) -> Result<()> {
    env_logger::init();

    block_on(async {
        let config = WorldConfig {
            storage_directory: None,
            debug_connectors: true,
            ..WorldConfig::default()
        };
        let (world, _handle) = World::start(config).await.unwrap();
        tremor_runtime::load_troy_file(&world, file_name)
            .await
            .unwrap();

        async_std::task::sleep(std::time::Duration::from_millis(150_000)).await;
        world.stop(ShutdownMode::Graceful).await.unwrap();
    });

    Ok(())
}

pub(crate) fn run_cmd(matches: &ArgMatches) -> Result<()> {
    let script_file = matches
        .value_of("SCRIPT")
        .ok_or_else(|| Error::from("No script file provided"))?;

    let args = if let Some(args_file) = matches.value_of("ARGS_FILE") {
        slurp_string(args_file)?
    } else {
        "{}".to_string()
    };
    let env = env::setup()?;
    let args = Script::parse(&env.module_path, "args.tremor", args, &env.fun)?;
    let helper = Helper::new(&env.fun, &env.aggr, vec![]);
    let args: std::result::Result<Value, tremor_script::errors::Error> =
        tremor_script::run_script(&helper, args.script.suffix());

    let mut args = if let Ok(args) = args {
        if !args.is_object() {
            return Err(Error::from(
                "Expected arguments file to contain a JSON record structure",
            ));
        }
        args
    } else {
        return Err(Error::from(
            "Expected arguments tremor file to be a valid tremor document",
        ));
    };

    if let Value::Object(ref mut fields) = args {
        if let Some(overags) = matches.values_of("arg") {
            for arg in overags {
                let kv: Vec<String> = arg.split('=').map(ToString::to_string).collect();
                if kv.len() != 2 {
                    // TODO output a nicer error
                }
                let key = kv.get(0);
                let value = kv.get(1);
                if let Some(key) = key {
                    if let Some(value) = value {
                        let mut json = value.clone();
                        let json: std::result::Result<Value, simd_json::Error> =
                            simd_json::from_str(&mut json);
                        if let Ok(json) = json {
                            let json: Value<'static> = json.clone_static();
                            fields.insert(Cow::owned(key.to_string()), json);
                        } else {
                            fields.insert(Cow::owned(key.to_string()), literal!(value.to_string()));
                        };
                    }
                }
            }
        }
    }

    let script_file = script_file.to_string();
    match get_source_kind(&script_file) {
        SourceKind::Troy => run_troy_source(matches, &script_file),
        SourceKind::Trickle => run_trickle_source(matches, &script_file, &args),
        SourceKind::Tremor | SourceKind::Json => run_tremor_source(matches, script_file, &args),
        SourceKind::Unsupported(_) => {
            Err(format!("Error: Unable to execute source: {}", &script_file).into())
        }
    }
}
