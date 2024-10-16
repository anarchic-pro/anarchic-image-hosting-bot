use actix_multipart::Multipart;
use actix_web::{post, web, App, HttpResponse, HttpServer, Responder};
use futures_util::stream::StreamExt as _;
use serde::Deserialize;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use teloxide::prelude::*;
use teloxide::types::{InputFile, ChatId};
use tokio::sync::Semaphore;
use uuid::Uuid;
use json5;
use log::{debug, error, info};

#[derive(Deserialize)]
struct Config {
    telegram_bot_token: String,
    chat_id: i64,
    max_concurrent_uploads: usize,
    host: String,
    port: String,
}

// Implement a custom Debug for Config to hide the telegram_bot_token
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")            
            .field("max_concurrent_uploads", &self.max_concurrent_uploads)
            .finish()
    }
}

// Upload the image to Telegram and return the image URL
async fn upload_to_telegram(file_path: &Path, bot: Bot, chat_id: ChatId) -> Result<String, Box<dyn std::error::Error>> {
    debug!("Uploading file: {:?} to Telegram chat: {:?}", file_path, chat_id);
    
    let response = bot.send_photo(chat_id, InputFile::file(file_path)).await?;
    let file = response.photo()
        .ok_or("No photo in response")?
        .last()
        .ok_or("Photo array is empty")?
        .file
        .clone();
    
    let file_id = file.id.clone();
    debug!("File uploaded to Telegram, received file ID: {:?}", file_id);
    
    // Get the file path
    let file_path = bot.get_file(&file_id).await?.path;
    
    let file_url = format!("https://api.telegram.org/file/bot{}/{}", bot.token(), file_path);
    debug!("Generated file URL: {}", file_url);
    Ok(file_url)
}

// Save the file locally with a unique UUID-based filename
async fn save_file(mut payload: Multipart) -> Result<String, actix_web::Error> {
    let mut file_path = String::new();

    while let Some(item) = payload.next().await {
        let mut field = item?;
        let content_disposition = field.content_disposition().unwrap();
        let filename = content_disposition.get_filename().unwrap();
        debug!("Received file: {:?}", filename);

        // Generate a unique filename
        let unique_id = Uuid::new_v4();
        let sanitized_filename = sanitize_filename::sanitize(&filename);
        let filepath = format!("C:/webtemp/{}_{}", unique_id, sanitized_filename);

        // Create and write to the file
        match File::create(&filepath) {
            Ok(mut f) => {
                while let Some(chunk) = field.next().await {
                    let data = chunk?;
                    f.write_all(&data).map_err(actix_web::error::ErrorInternalServerError)?;
                }
                file_path = filepath;
                info!("File created successfully: {:?}", file_path);
            }
            Err(e) => {
                error!("Failed to create file: {:?}", e);
                return Err(actix_web::error::ErrorInternalServerError(e));
            }
        }
    }

    if file_path.is_empty() {
        error!("File path is empty, failed to save file");
        return Err(actix_web::error::ErrorInternalServerError("File path is empty"));
    }

    Ok(file_path)
}

#[post("/upload")]
async fn upload(
    payload: Multipart,
    data: web::Data<UploadData>,
) -> impl Responder {
    let bot = data.bot.clone();
    let chat_id = data.chat_id;

    debug!("Starting upload process for chat ID: {:?}", chat_id);

    // Save the uploaded file
    match save_file(payload).await {
        Ok(file_path) => {
            let path = Path::new(&file_path);
            debug!("File saved locally at: {:?}", path);

            // Semaphore to limit concurrent uploads
            let permit = data.semaphore.acquire().await.unwrap();
            let result = upload_to_telegram(path, bot, chat_id).await;

            drop(permit); // Release semaphore permit

            // Remove the temporary file
            if let Err(e) = std::fs::remove_file(path) {
                error!("Failed to delete temporary file: {:?}", e);
            }

            match result {
                Ok(url) => {
                    debug!("Successfully uploaded image to Telegram, URL: {}", url);
                    HttpResponse::Ok().body(format!("{}", url))
                }
                Err(e) => {
                    error!("Failed to upload image to Telegram: {:?}", e);
                    HttpResponse::InternalServerError().body(format!("Failed to upload image: {:?}", e))
                }
            }
        }
        Err(e) => {
            error!("Failed to save file: {:?}", e);
            HttpResponse::InternalServerError().body(format!("Failed to save file: {:?}", e))
        }
    }
}

// Struct to hold shared data for the upload handler
struct UploadData {
    bot: Bot,
    chat_id: ChatId,
    semaphore: Semaphore,
}

// Read configuration from a JSON5 file
fn read_config(config_file: &str) -> Config {
    let file_content = std::fs::read_to_string(config_file).expect("Failed to read config file");
    json5::from_str(&file_content).expect("Failed to parse config")
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Initialize logger
    env_logger::builder().filter_level(log::LevelFilter::Debug).init();
    info!("Starting server...");

    let config = read_config("anarchic-image-hosting-bot.json5");

    // Never log the telegram_bot_token for security reasons
    debug!("Configuration loaded: {:?}", config);

    // Initialize the bot
    let bot = Bot::new(config.telegram_bot_token.clone());

    let semaphore = Semaphore::new(config.max_concurrent_uploads);
    let upload_data = web::Data::new(UploadData {
        bot: bot.clone(),
        chat_id: ChatId(config.chat_id),
        semaphore,
    });

    // Start the Actix web server with the host and port from the config
    let bind_address = format!("{}:{}", config.host, config.port);
    HttpServer::new(move || {
        App::new()
            .app_data(upload_data.clone())
            .service(upload)
    })
    .bind(&bind_address)?
    .run()
    .await
}
