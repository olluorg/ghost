//! Клиент к OpenAI-совместимому API со стримингом.
//!
//! Намеренно на блокирующем `reqwest`, а не на tokio: вся подсистема и так
//! живёт в своём потоке, а `Response` реализует `Read` — построчный разбор SSE
//! получается прямым и без асинхронной обвязки.

use std::io::{BufRead, BufReader};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::json;

use super::{Role, Turn, Usage};

/// Параметры одного запроса. Модель и бюджет меняются от хода к ходу: снимок
/// экрана уходит в модель со зрением, обычная реплика — в текстовую.
pub struct Req {
    pub model: String,
    pub max_tokens: u32,
    /// `None` — не отправлять поле вовсе. Часть моделей требует рассуждение
    /// принудительно и отвечает 400 на попытку его выключить.
    pub reasoning: Option<bool>,
}

pub struct Client {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self> {
        if api_key.is_empty() {
            bail!("не задан ключ API (ROUTERAI_KEY в .env)");
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("http-клиент")?;

        Ok(Self { http, base_url: base_url.to_string(), api_key: api_key.to_string() })
    }

    /// Шлёт запрос и отдаёт куски ответа по мере поступления.
    pub fn stream(
        &self,
        req: &Req,
        system: &str,
        turns: &[Turn],
        mut on_delta: impl FnMut(String) -> bool,
    ) -> Result<Usage> {
        match self.attempt(req, req.reasoning, system, turns, &mut on_delta) {
            // Некоторые модели не дают выключить рассуждение. Узнать это заранее
            // нельзя, поэтому просто повторяем без параметра — ни один токен
            // ответа к этому моменту ещё не пришёл, терять нечего.
            Err(e) if e.to_string().contains("Reasoning is mandatory") => {
                self.attempt(req, None, system, turns, &mut on_delta)
            }
            other => other,
        }
    }

    fn attempt(
        &self,
        req: &Req,
        reasoning: Option<bool>,
        system: &str,
        turns: &[Turn],
        on_delta: &mut dyn FnMut(String) -> bool,
    ) -> Result<Usage> {
        let mut messages = vec![json!({ "role": "system", "content": system })];
        for turn in turns {
            let role = match turn.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content = match &turn.image {
                Some(png) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(png.as_slice());
                    json!([
                        { "type": "image_url",
                          "image_url": { "url": format!("data:image/png;base64,{b64}") } },
                        { "type": "text", "text": turn.text },
                    ])
                }
                None => json!(turn.text),
            };
            messages.push(json!({ "role": role, "content": content }));
        }

        let mut body = json!({
            "model": req.model,
            "stream": true,
            "max_tokens": req.max_tokens,
            // Без этого расход приходит только в непотоковом ответе, а нам
            // нужен и он, и потоковая выдача одновременно.
            "stream_options": { "include_usage": true },
            "messages": messages,
        });
        if let Some(enabled) = reasoning {
            body["reasoning"] = json!({ "enabled": enabled });
        }

        let endpoint = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("запрос не ушёл")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("{status}: {}", body.chars().take(300).collect::<String>());
        }

        let mut usage = Usage::default();
        for line in BufReader::new(response).lines() {
            let line = line.context("обрыв потока")?;
            let Some(payload) = line.strip_prefix("data:") else {
                continue; // пустые строки-разделители и поля вроде event:
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                break;
            }

            let parsed: serde_json::Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                // Единичный битый кадр не повод ронять весь ответ.
                Err(_) => continue,
            };
            if let Some(err) = parsed.get("error") {
                bail!("{err}");
            }
            // Расход приходит отдельным кадром в самом конце.
            if let Some(u) = parsed.get("usage").filter(|u| u.is_object()) {
                usage = Usage {
                    input: u["prompt_tokens"].as_u64().unwrap_or(0),
                    output: u["completion_tokens"].as_u64().unwrap_or(0),
                    cost: u["cost"].as_f64().unwrap_or(0.0),
                };
            }
            if let Some(text) = parsed["choices"][0]["delta"]["content"].as_str() {
                // Ответ false означает «этот ответ больше не нужен»: выходим,
                // и соединение закрывается вместе с телом.
                if !text.is_empty() && !on_delta(text.to_string()) {
                    break;
                }
            }
        }

        Ok(usage)
    }
}
