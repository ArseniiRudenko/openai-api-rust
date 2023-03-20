pub mod chat;
pub mod completion;
pub mod edit;
pub mod structs;
pub mod image;
pub mod files;
pub mod embeddings;
pub mod fine_tunes;
pub mod moderations;
mod audio;

use anyhow::Result;
use std::io;
use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use std::pin::Pin;
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{Body, Client, multipart, RequestBuilder};
use reqwest::multipart::Part;
use serde::de::DeserializeOwned;
use serde::ser::StdError;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio_stream::{Stream, StreamExt};
use tokio_util::codec::{BytesCodec, FramedRead};
use with_id::WithRefId;
use crate::structs::{ApiResponse, Model, ModelRequest, ModelsResponse};

#[derive(Debug, Clone)]
pub struct OpenAiClient {
    url:String,
    key:String,
    client:Client
}

impl OpenAiClient {

    const URL: &'static str = "https://api.openai.com/v1";

    pub fn new(key: &str)->Self{
        let client = Client::new();
        OpenAiClient::with_client(key,&client)
    }

    /// reqwest library recommends reusing single client,
    /// so if you run access to multiple api-s, pass client into constructor
    pub fn with_client(key: &str, client: &Client)->Self{
        OpenAiClient::with_url_and_client(key,OpenAiClient::URL,client)
    }

    pub fn with_url(key: &str, url: &str) -> Self {
        let client = Client::new();
        OpenAiClient::with_url_and_client(key,url,&client)
    }


    pub fn with_url_and_client(key: &str, url: &str, client: &Client)->Self{
        OpenAiClient {
            url: url.to_string(),
            key: key.to_string(),
            client: client.clone()
        }
    }
}


#[async_trait]
pub trait JsonRequest<TRes: DeserializeOwned>: Serialize + Sized + Sync{

    const ENDPOINT: &'static str;

    async fn run(&self, client:&OpenAiClient) -> Result<ApiResponse<TRes>>{
        let final_url = client.url.to_owned()+Self::ENDPOINT;
        let res = client.client.post(final_url)
            .bearer_auth(client.key.clone())
            .json(self)
            .send()
            .await?
            .json::<ApiResponse<TRes>>()
            .await?;
        Ok(res)
    }
}


#[async_trait]
pub trait ByUrlRequest<TRes: DeserializeOwned>:WithRefId<str>+Sync{

    const ENDPOINT: &'static str;
    const SUFFIX: &'static str;

    fn builder(client:&OpenAiClient,final_url:String)->RequestBuilder{
        client.client.get(final_url)
    }

    async fn run(&self, client:&OpenAiClient)-> Result<ApiResponse<TRes>>{
        let final_url = client.url.to_owned()+Self::ENDPOINT+self.id()+Self::SUFFIX;
        let res = Self::builder(client,final_url)
            .bearer_auth(client.key.clone())
            .send()
            .await?
            .json::<ApiResponse<TRes>>()
            .await?;
        Ok(res)
    }
}


#[async_trait]
pub trait GetRequest:DeserializeOwned {

    const ENDPOINT: &'static str;

    async fn get(client:&OpenAiClient)-> Result<ApiResponse<Self>>{
        let final_url = client.url.to_owned()+Self::ENDPOINT;
        let res = client.client.get(final_url)
            .bearer_auth(client.key.clone())
            .send()
            .await?
            .json::<ApiResponse<Self>>()
            .await?;
        Ok(res)
    }
}

#[async_trait]
pub trait FormRequest<TRes: DeserializeOwned> : AsyncTryInto<multipart::Form>+Clone+Sync+Send {

    const ENDPOINT: &'static str;

    async fn run(&self, client:&OpenAiClient)-> Result<ApiResponse<TRes>>{
        let final_url =  client.url.to_owned()+Self::ENDPOINT;
        let res = client.client.post(final_url)
            .bearer_auth(client.key.clone())
            .multipart(AsyncTryInto::try_into(self.clone()).await?)
            .send()
            .await?
            .json::<ApiResponse<TRes>>()
            .await?;
        Ok(res)
    }
}

#[async_trait(?Send)]
pub trait DownloadRequest: WithRefId<str>{

    const ENDPOINT: &'static str;
    const SUFFIX: &'static str = "";

    async fn download(&self, client:&OpenAiClient) -> Result<Pin<Box<dyn Stream<Item=Result<Bytes, reqwest::Error>>>>>{
        let final_url = client.url.to_owned()+Self::ENDPOINT+self.id()+Self::SUFFIX;
        let res = client.client.get(final_url)
            .bearer_auth(client.key.clone())
            .send()
            .await?
            .error_for_status()?
            .bytes_stream();
        Ok(Box::pin(res))
    }

    async fn download_to_file(&self, client:&OpenAiClient, target_path:&str) -> Result<()>{
        let mut file = File::create(target_path).await?;
        let mut stream = self.download(client).await?;
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
        }
        Ok(())
    }

}


impl GetRequest for ModelsResponse {
    const ENDPOINT: &'static str = "/models";
}

impl ByUrlRequest<Model> for ModelRequest{
    const ENDPOINT: &'static str = "/models/";
    const SUFFIX: &'static str = "";
}


#[async_trait]
pub trait AsyncTryFrom<T>: Sized {

    type Error: 'static+StdError+Send+Sync;

    async fn try_from(value: T) -> Result<Self, Self::Error>;
}

#[async_trait]
pub trait AsyncTryInto<T>: Sized {

    type Error: 'static+StdError+Send+Sync;

    async fn try_into(self) -> Result<T, Self::Error>;
}

#[async_trait]
impl<T, U> AsyncTryInto<U> for T
    where
        U: AsyncTryFrom<T>,
        T: Send
{
    type Error = U::Error;

    async fn try_into(self) -> Result<U, Self::Error>{
        U::try_from(self).await
    }
}


pub(crate) async fn file_to_part(path: &PathBuf) -> io::Result<Part> {
    let name = path.file_name()
        .ok_or(Error::new(ErrorKind::InvalidInput,"filename is not full"))?
        .to_str()
        .ok_or(Error::new(ErrorKind::InvalidData,"non unicode filename"))?
        .to_owned();
    let file = File::open(path).await?;
    let size = file.metadata().await?.len();
    let stream = FramedRead::new(file, BytesCodec::new());
    let body = Body::wrap_stream(stream);
    Ok(Part::stream_with_length(body,size).file_name(name))
}