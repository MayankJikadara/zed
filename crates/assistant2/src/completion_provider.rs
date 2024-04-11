use anyhow::Result;
use client::{proto, Client};
use futures::{future::BoxFuture, stream::BoxStream, FutureExt, StreamExt};
use gpui::Global;
use std::sync::Arc;

pub enum CompletionRole {
    User,
    Assistant,
    System,
}

pub struct CompletionMessage {
    pub role: CompletionRole,
    pub body: String,
}

#[derive(Clone)]
pub struct CompletionProvider(Arc<dyn CompletionProviderBackend>);

impl CompletionProvider {
    pub fn new(backend: impl CompletionProviderBackend) -> Self {
        Self(Arc::new(backend))
    }

    pub fn complete(
        &self,
        model: String,
        messages: Vec<CompletionMessage>,
        stop: Vec<String>,
        temperature: f32,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<String>>>> {
        self.0.complete(model, messages, stop, temperature)
    }
}

impl Global for CompletionProvider {}

pub trait CompletionProviderBackend: 'static {
    fn complete(
        &self,
        model: String,
        messages: Vec<CompletionMessage>,
        stop: Vec<String>,
        temperature: f32,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<String>>>>;
}

pub struct CloudCompletionProvider {
    client: Arc<Client>,
}

impl CloudCompletionProvider {
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

impl CompletionProviderBackend for CloudCompletionProvider {
    fn complete(
        &self,
        model: String,
        messages: Vec<CompletionMessage>,
        stop: Vec<String>,
        temperature: f32,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<String>>>> {
        let client = self.client.clone();
        async move {
            let stream = client
                .request_stream(proto::CompleteWithLanguageModel {
                    model,
                    messages: messages
                        .into_iter()
                        .map(|message| proto::LanguageModelRequestMessage {
                            role: match message.role {
                                CompletionRole::User => {
                                    proto::LanguageModelRole::LanguageModelUser as i32
                                }
                                CompletionRole::Assistant => {
                                    proto::LanguageModelRole::LanguageModelAssistant as i32
                                }
                                CompletionRole::System => {
                                    proto::LanguageModelRole::LanguageModelSystem as i32
                                }
                            },
                            content: message.body,
                        })
                        .collect(),
                    stop,
                    temperature,
                })
                .await?;

            Ok(stream
                .filter_map(|response| async move {
                    match response {
                        Ok(mut response) => Some(Ok(response.choices.pop()?.delta?.content?)),
                        Err(error) => Some(Err(error)),
                    }
                })
                .boxed())
        }
        .boxed()
    }
}