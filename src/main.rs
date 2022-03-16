use std::{collections::HashMap, env::vars, sync::Arc};

use chrono::Utc;
use itertools::Itertools;
use log::{info, warn};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, Database, DatabaseConnection,
    EntityTrait, IntoActiveModel, Order, QueryFilter, QueryOrder, Schema, Set,
};
use teloxide::{
    dispatching2::UpdateFilterExt,
    prelude2::*,
    types::{InlineQueryResult, InlineQueryResultCachedSticker, ParseMode, Sticker},
    utils::command::BotCommand,
};

mod model;
mod strings;

#[tokio::main]
async fn main() -> Result<(), BotError> {
    // initialize logger with sane defaults
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "sticker_search=info,teloxide=error");
    }
    pretty_env_logger::init();

    info!("Starting bot");

    let bot = Bot::from_env();

    // get db url from environment
    let db_url = vars()
        .collect::<HashMap<_, _>>()
        .get("DB_URL")
        .expect("DB_URL to be set")
        .clone();

    // connect to db
    let db = Database::connect(db_url).await?;

    // create tables if not exists
    create_table(model::tagged_sticker::Entity, &db).await?;
    create_table(model::sticker::Entity, &db).await?;
    create_table(model::user::Entity, &db).await?;

    // setup handlers
    let inline_handler =
        Update::filter_inline_query().branch(dptree::endpoint(inline_query_handler));
    let cmd_handler = Update::filter_message()
        .filter_command::<Command>()
        .branch(dptree::endpoint(command_handler));
    let feedback_handler = Update::filter_chosen_inline_result()
        .branch(dptree::endpoint(chosen_inline_result_handler));

    let handler = dptree::entry()
        .branch(inline_handler)
        .branch(cmd_handler)
        .branch(feedback_handler);

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![Arc::new(DataStore::new(db))])
        .build()
        .setup_ctrlc_handler()
        .dispatch()
        .await;

    Ok(())
}

async fn create_table<E: EntityTrait>(entity: E, db: &DatabaseConnection) -> Result<(), BotError> {
    let builder = db.get_database_backend();
    let schema = Schema::new(builder);

    db.execute(builder.build(schema.create_table_from_entity(entity).if_not_exists()))
        .await?;

    Ok(())
}

struct DataStore {
    db: DatabaseConnection,
    // secret for admin operations; read from environment variables
    secret: String,
}

impl DataStore {
    fn new(db: DatabaseConnection) -> Self {
        let vars = vars().collect::<HashMap<_, _>>();
        let secret = vars
            .get("STICKERS_SECRET")
            .expect("STICKERS_SECRET to be set")
            .clone();
        Self { db, secret }
    }
}

async fn chosen_inline_result_handler(
    _bot: Bot,
    chosen: ChosenInlineResult,
    store: Arc<DataStore>,
) -> Result<(), BotError> {
    let sticker_id: i32 = chosen
        .result_id
        .parse()
        .map_err(|_| BotError::ChosenParseError)?;

    let sticker = model::sticker::Entity::find()
        .filter(model::sticker::Column::Id.eq(sticker_id))
        .one(&store.db)
        .await?;

    if let Some(sticker) = sticker {
        let new_popularity: i64 = sticker.popularity + 1;
        let mut active_sticker = sticker.into_active_model();
        active_sticker.popularity = Set(new_popularity);
        active_sticker.update(&store.db).await?;
    } else {
        warn!("Chosen sticker id {sticker_id} not found in database")
    }

    Ok(())
}

async fn command_handler(
    bot: Bot,
    message: Message,
    store: Arc<DataStore>,
) -> Result<(), BotError> {
    let command = Command::parse(
        message.text().ok_or(BotError::CommandParseError(None))?,
        "sticker_doko_bot",
    )?;

    match command {
        Command::Tag { text } => {
            let re_msg: &Message = match message.reply_to_message() {
                Some(m) => m,
                None => {
                    info!(
                        "/tag command by {} does not reply to a message",
                        username_of_message(&message, "<unknown>")
                    );

                    reply_msg(bot, message, strings::NO_REPLY_STICKER).await?;
                    return Ok(());
                }
            };

            // only process tag requests from known senders
            let sender = match message.from() {
                Some(user) => user,
                None => {
                    info!("Unknown user attempted to use the /tag command");

                    reply_msg(bot, message, strings::SENDER_UNKNOWN).await?;
                    return Ok(());
                }
            };

            // check if sender is known
            let db_user = model::user::Entity::find()
                .filter(model::user::Column::UserId.eq(sender.id))
                .one(&store.db)
                .await?;
            let db_user = if let Some(u) = db_user {
                u
            } else {
                info!(
                    "Unregistered user {} attempted to use the /tag command",
                    username_of_message(&message, "<unknown>")
                );

                reply_msg(bot, message, strings::TAG_NOT_AUTHORIZED).await?;
                return Ok(());
            };

            // check if sender is allowed to tag
            if db_user.allowed == false {
                info!(
                    "Non-allowed tagger {} attempted to use the /tag command",
                    username_of_message(&message, "<unknown>")
                );

                reply_msg(bot, message, strings::TAG_NOT_AUTHORIZED).await?;
                return Ok(());
            }

            /* Proceed to tag */

            // prepare data to be inserted
            let re_sticker: &Sticker = match re_msg.sticker() {
                Some(s) => s,
                None => {
                    info!(
                        "/tag command by {} does not reply to a sticker",
                        db_user.username
                    );

                    reply_msg(bot, message, strings::NO_REPLY_STICKER).await?;
                    return Ok(());
                }
            };

            // ensure that there's a set name
            let set_name = match &re_sticker.set_name {
                Some(name) => name,
                None => {
                    info!("Sticker {:?} does not have a sticker set", re_sticker);

                    reply_msg(bot, message, strings::NO_STICKER_SET).await?;
                    return Ok(());
                }
            };
            let file_id = &re_sticker.file_id;
            let file_unique_id = &re_sticker.file_unique_id;
            let tags: Vec<_> = text.trim().split_whitespace().collect();

            if tags.is_empty() {
                info!(
                    "Tagger {} used /tag command without any tags",
                    db_user.username
                );

                reply_msg(bot, message, strings::NO_TAGS).await?;
                return Ok(());
            }

            // ensure that the sticker is indexed
            // NOTE: This is a workaround to implement the "insert if not exists" behavior
            let inserted_sticker_res =
                model::sticker::Entity::insert(model::sticker::ActiveModel {
                    file_unique_id: Set(file_unique_id.clone()),
                    file_id: Set(file_id.clone()),
                    set_name: Set(set_name.clone()),
                    popularity: Set(0),
                    ..Default::default()
                })
                .exec(&store.db)
                .await;

            // get the inserted id, or else fallback to selecting
            let sticker_id: i32 = match inserted_sticker_res {
                Ok(sticker) => sticker.last_insert_id,
                Err(_) => {
                    let sticker = model::sticker::Entity::find()
                        .filter(model::sticker::Column::FileUniqueId.eq(file_unique_id.clone()))
                        .one(&store.db)
                        .await?;
                    sticker.ok_or(BotError::NoSuchStickerError)?.id
                }
            };

            // map tag strings to tag entries
            let tagged_stickers = tags.iter().map(|tag| model::tagged_sticker::ActiveModel {
                tag: Set(tag.to_string()),
                sticker_id: Set(sticker_id),
                tagger_id: Set(db_user.id),
                ts: Set(Utc::now()),
                ..Default::default()
            });

            // insert to db
            let _insert_res = model::tagged_sticker::Entity::insert_many(tagged_stickers)
                .exec(&store.db)
                .await?;

            info!(
                "{username} tagged sticker with file_unique_id {file_unique_id} in set {set_name} with tags: {tags:?}",
                username = db_user.username
            );

            // respond to user with what's being tagged
            let tags_joined = tags.iter().join("\n- ");
            reply_msg(
                bot,
                message,
                format!(
                    "{prefix}\n- {tags_joined}",
                    prefix = strings::TAGGED_STICKER
                ),
            )
            .await?;
        }
        Command::Untag { text } => {
            let re_msg: &Message = match message.reply_to_message() {
                Some(m) => m,
                None => {
                    info!(
                        "/tag command by {} does not reply to a message",
                        username_of_message(&message, "<unknown>")
                    );

                    reply_msg(bot, message, strings::NO_REPLY_STICKER).await?;
                    return Ok(());
                }
            };

            // only process tag requests from known senders
            let sender = match message.from() {
                Some(user) => user,
                None => {
                    info!("Unknown user attempted to use the /tag command");

                    reply_msg(bot, message, strings::SENDER_UNKNOWN).await?;
                    return Ok(());
                }
            };

            // check if sender is known
            let db_user = model::user::Entity::find()
                .filter(model::user::Column::UserId.eq(sender.id))
                .one(&store.db)
                .await?;
            let db_user = if let Some(u) = db_user {
                u
            } else {
                info!(
                    "Unregistered user {} attempted to use the /tag command",
                    username_of_message(&message, "<unknown>")
                );

                reply_msg(bot, message, strings::TAG_NOT_AUTHORIZED).await?;
                return Ok(());
            };

            // check if sender is allowed to tag
            if db_user.allowed == false {
                info!(
                    "Non-allowed tagger {} attempted to use the /tag command",
                    username_of_message(&message, "<unknown>")
                );

                reply_msg(bot, message, strings::TAG_NOT_AUTHORIZED).await?;
                return Ok(());
            }

            /* Proceed to tag */

            // prepare data to be inserted
            let re_sticker: &Sticker = match re_msg.sticker() {
                Some(s) => s,
                None => {
                    info!(
                        "/tag command by {} does not reply to a sticker",
                        db_user.username
                    );

                    reply_msg(bot, message, strings::NO_REPLY_STICKER).await?;
                    return Ok(());
                }
            };

            let file_unique_id = &re_sticker.file_unique_id;
            let untags: Vec<_> = text.trim().split_whitespace().collect();

            let sticker = model::sticker::Entity::find()
                .filter(model::sticker::Column::FileUniqueId.eq(file_unique_id.clone()))
                .one(&store.db)
                .await?;
            let sticker_id = match sticker {
                Some(sticker) => sticker.id,
                None => {
                    info!("Tagger {username} used /untag against an unindexed sticker with unique id {file_unique_id}",
                    username = db_user.username);

                    reply_msg(bot, message, strings::STICKER_UNTAGGED).await?;
                    return Ok(());
                }
            };

            let delete_res = model::tagged_sticker::Entity::delete_many()
                .filter(model::tagged_sticker::Column::StickerId.eq(sticker_id))
                .filter(model::tagged_sticker::Column::Tag.is_in(untags.clone()))
                .exec(&store.db)
                .await?;

            info!(
                "Tagger {username} removed tags {untags:?} from sticker with unique id {file_unique_id} (deleted {rows} rows)",
                username = db_user.username, rows = delete_res.rows_affected
            );
            reply_msg(bot, message, strings::UNTAG_SUCCESS).await?;
        }
        Command::ListTags => {
            let re_msg: &Message = match message.reply_to_message() {
                Some(m) => m,
                None => {
                    info!(
                        "User {} used /listtags without replying to a sticker",
                        username_of_message(&message, "<unknown>")
                    );

                    reply_msg(bot, message, strings::NO_REPLY_STICKER).await?;
                    return Ok(());
                }
            };

            let re_sticker: &Sticker = match re_msg.sticker() {
                Some(s) => s,
                None => {
                    info!(
                        "User {} used /listtags command without replying to a sticker",
                        username_of_message(&message, "<unknown>")
                    );

                    reply_msg(bot, message, strings::NO_REPLY_STICKER).await?;
                    return Ok(());
                }
            };
            let file_unique_id = &re_sticker.file_unique_id;
            info!("Finding sticker with unique_file_id: {file_unique_id}");

            let sticker = model::sticker::Entity::find()
                .filter(model::sticker::Column::FileUniqueId.eq(file_unique_id.clone()))
                .one(&store.db)
                .await?;
            let sticker_id = match sticker {
                Some(sticker) => sticker.id,
                None => {
                    info!(
                        "User {} used /listtags against an unindexed sticker with unique id {file_unique_id}",
                        username_of_message(&message, "<unknown>")
                    );

                    reply_msg(bot, message, strings::STICKER_UNTAGGED).await?;
                    return Ok(());
                }
            };

            let tagged_stickers = model::tagged_sticker::Entity::find()
                .filter(model::tagged_sticker::Column::StickerId.eq(sticker_id))
                .all(&store.db)
                .await?;

            if tagged_stickers.is_empty() {
                info!(
                    "User {} used /listtags against an indexed, but untagged sticker with unique id {file_unique_id}",
                    username_of_message(&message, "<unknown>")
                );

                reply_msg(bot, message, strings::STICKER_UNTAGGED).await?;
                return Ok(());
            }

            let tags = tagged_stickers.into_iter().map(|ts| ts.tag).join(" ");

            reply_msg(bot, message, format!("Tags on this sticker: {}", tags)).await?;
        }
        Command::Register => {
            // only process register requests from known senders
            let sender = match message.from() {
                Some(user) => user,
                None => {
                    reply_msg(bot, message, strings::SENDER_UNKNOWN).await?;
                    return Ok(());
                }
            };

            // the table requires username
            let username = if let Some(username) = &sender.username {
                username.clone()
            } else {
                info!("A user {sender:?} without username attempted to register");

                reply_msg(bot, message, strings::USERNAME_MISSING).await?;
                return Ok(());
            };

            let _insert_res = model::user::Entity::insert(model::user::ActiveModel {
                username: Set(username),
                user_id: Set(sender.id),
                allowed: Set(false),
                ..Default::default()
            })
            .exec(&store.db)
            .await?;

            // respond to user
            reply_msg(bot, message, strings::NEED_APPROVAL).await?;
        }
        Command::Allow { text } => {
            let args = text.trim().split_whitespace().collect_vec();
            if args.len() != 2 {
                reply_msg(bot, message, strings::WRONG_ARGNUM).await?;
                return Ok(());
            }
            let (secret, username) = (args[0], args[1]);

            // verify secret
            if secret != store.secret {
                reply_msg(bot, message, strings::NO_PERM).await?;
                return Ok(());
            }

            // query the username
            let user = model::user::Entity::find()
                .filter(model::user::Column::Username.eq(username))
                .one(&store.db)
                .await?;

            let user = if let Some(u) = user {
                u
            } else {
                reply_msg(bot, message, strings::NOT_REGISTERED).await?;
                return Ok(());
            };

            // update the user
            let mut user_active = user.into_active_model();
            user_active.allowed = Set(true);
            let updated_user = user_active.update(&store.db).await?;

            format!("{:?}", updated_user);
            reply_msg_with_parse_mode(
                bot,
                message,
                Some(ParseMode::Html),
                format!("Updated user: <code>{:?}</code>", updated_user),
            )
            .await?;
        }
        Command::Help => {
            reply_msg(bot, message, Command::descriptions()).await?;
        }
    }
    Ok(())
}

async fn inline_query_handler(
    bot: Bot,
    update: InlineQuery,
    store: Arc<DataStore>,
) -> Result<(), BotError> {
    let query_str = update.query.as_str();

    // reject empty queries
    if query_str.trim() == "" {
        return Ok(());
    }

    info!("Query: {query_str}");

    // construct query condition
    let queries = query_str.trim().split_whitespace().collect_vec();
    let mut condition = Condition::any();
    for query in queries {
        condition = condition.add(model::tagged_sticker::Column::Tag.contains(query));
    }

    // query sticker ids
    let mut sticker_ids = model::tagged_sticker::Entity::find()
        .filter(condition)
        .all(&store.db)
        .await?
        .into_iter()
        .map(|tagged_sticker| tagged_sticker.sticker_id)
        .collect_vec();

    // sort & dedup sticker ids
    sticker_ids.sort();
    sticker_ids.dedup();

    // convert sticker ids to file ids, ordered by popularity, descending
    let sticker_file_id_pairs = model::sticker::Entity::find()
        .filter(model::sticker::Column::Id.is_in(sticker_ids))
        .order_by(model::sticker::Column::Popularity, Order::Desc)
        .all(&store.db)
        .await?
        .into_iter()
        .map(|sticker| (sticker.id, sticker.file_id))
        .collect_vec();

    // The sticker id's in database is used as unique identifiers.
    // The identifiers are then used in the chosen result handler to collect usage statistics
    let query_responses = sticker_file_id_pairs
        .into_iter()
        .map(|(sticker_id, file_id)| {
            InlineQueryResultCachedSticker::new(sticker_id.to_string(), file_id).into()
        })
        .collect::<Vec<InlineQueryResult>>();

    bot.answer_inline_query(update.id, query_responses)
        .send()
        .await?;

    Ok(())
}

async fn reply_msg<S: AsRef<str>>(bot: Bot, message: Message, text: S) -> Result<(), BotError> {
    reply_msg_with_parse_mode(bot, message, None, text).await?;
    Ok(())
}

async fn reply_msg_with_parse_mode<S: AsRef<str>>(
    bot: Bot,
    message: Message,
    parse_mode: Option<ParseMode>,
    text: S,
) -> Result<(), BotError> {
    let mut send_message = bot.send_message(message.chat.id, text.as_ref());
    send_message.reply_to_message_id = Some(message.id);
    send_message.parse_mode = parse_mode;
    send_message.send().await?;
    Ok(())
}

fn username_of_message<'a>(message: &'a Message, fallback: &'a str) -> &'a str {
    message
        .from()
        .and_then(|u| u.username.as_ref())
        .map(|s| s.as_str())
        .unwrap_or_else(|| fallback.as_ref())
}

#[derive(BotCommand, Debug)]
#[command(rename = "lowercase", description = "Commands:")]
enum Command {
    #[command(description = "tag a sticker with text description")]
    Tag { text: String },

    #[command(description = "register self as a tagger")]
    Register,

    #[command(description = "allow a user to tag")]
    Allow { text: String },

    #[command(description = "get help message")]
    Help,

    #[command(description = "remove a tag from a sticker")]
    Untag { text: String },

    #[command(description = "list all tags associated with a sticker")]
    ListTags,
}

#[derive(Debug)]
enum BotError {
    /// Problem originated from the Telegram bot library
    RequestError(teloxide::RequestError),

    /// Command parsing error
    CommandParseError(Option<teloxide::utils::command::ParseError>),

    /// Problem originated from the database library
    DatabaseError(Option<sea_orm::DbErr>),

    /// Problem parsing the `result_id` field of [`ChosenInlineResult`] as a sticker ID
    ChosenParseError,

    /// Problem inserting and finding the sticker
    NoSuchStickerError,
}

impl From<teloxide::RequestError> for BotError {
    fn from(e: teloxide::RequestError) -> Self {
        Self::RequestError(e)
    }
}

impl From<teloxide::utils::command::ParseError> for BotError {
    fn from(e: teloxide::utils::command::ParseError) -> Self {
        Self::CommandParseError(Some(e))
    }
}

impl From<sea_orm::DbErr> for BotError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::DatabaseError(Some(e))
    }
}

impl std::fmt::Display for BotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestError(e) => write!(f, "{:?}", e),
            Self::CommandParseError(Some(e)) => write!(f, "{:?}", e),
            Self::CommandParseError(None) => write!(f, "CommandParseError"),
            Self::DatabaseError(Some(e)) => write!(f, "{:?}", e),
            Self::DatabaseError(None) => write!(f, "DatabaseError"),
            Self::ChosenParseError => write!(f, "ChosenParseError"),
            Self::NoSuchStickerError => write!(f, "NoSuchStickerError"),
        }
    }
}

impl std::error::Error for BotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RequestError(e) => Some(e),
            Self::CommandParseError(Some(e)) => Some(e),
            Self::DatabaseError(Some(e)) => Some(e),
            _ => None,
        }
    }
}
