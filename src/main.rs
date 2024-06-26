use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use action::{MessageAction, ReactionAction};
use chrono::{DateTime, Utc};
use filter::SpamHistory;
use influxdb::{InfluxDbWriteable, WriteQuery};
use reqwest::header::HeaderValue;
use tokio::sync::RwLock;

use futures::stream::StreamExt;

use tracing::Instrument;

use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::Event;
use twilight_gateway::Shard;
use twilight_http::Client as HttpClient;
use twilight_mention::Mention;
use twilight_model::application::interaction::InteractionData;
use twilight_model::channel::Message;
use twilight_model::gateway::payload::incoming::MessageUpdate;
use twilight_model::gateway::{GatewayReaction, Intents};
use twilight_model::id::marker::ApplicationMarker;
use twilight_model::id::{marker::GuildMarker, Id};

use color_eyre::eyre::Result;

use config::*;
use model::{MessageInfo, ReactionInfo};
use twilight_util::builder::embed::{EmbedBuilder, EmbedFieldBuilder};

mod action;
mod command;
mod config;
mod confusable;
mod filter;
mod message;
mod model;
mod reaction;

const DEFAULT_RELOAD_INTERVAL: u64 = 5 * 60;

#[derive(Clone, Debug)]
struct State {
    cfg: Arc<Config>,
    guild_cfgs: Arc<RwLock<HashMap<Id<GuildMarker>, GuildConfig>>>,
    http: Arc<HttpClient>,
    application_id: Arc<RwLock<Option<Id<ApplicationMarker>>>>,
    cache: Arc<InMemoryCache>,
    spam_history: Arc<RwLock<SpamHistory>>,
    influx_client: Arc<Option<influxdb::Client>>,
    influx_report_count: Arc<AtomicUsize>,
    armed: Arc<AtomicBool>,
}

#[derive(Debug, InfluxDbWriteable)]
struct EventTimingReport {
    time: DateTime<Utc>,
    guild: String,
    channel: String,
    time_taken: f64,
    #[influxdb(tag)]
    action_kind: &'static str,
    #[influxdb(tag)]
    development: bool,
}

#[derive(Debug, InfluxDbWriteable)]
struct MessageFilterReport {
    time: DateTime<Utc>,
    guild: String,
    channel: String,
}

#[derive(Debug, InfluxDbWriteable)]
struct ReactionFilterReport {
    time: DateTime<Utc>,
    guild: String,
    channel: String,
}

#[cfg(debug_assertions)]
fn init_tracing() {
    tracing_subscriber::fmt()
        .pretty()
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("chrysanthemum=trace".parse().unwrap()),
        )
        .init();
}

#[cfg(not(debug_assertions))]
fn init_tracing() {
    use tracing_subscriber::prelude::*;

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json())
        .with(sentry_tracing::layer())
        .init();
}

async fn send_influx_point(state: &State, point: &WriteQuery) -> Result<()> {
    if let Some(influx_client) = state.influx_client.as_ref() {
        if let Some(influx_cfg) = state.cfg.influx.as_ref() {
            let count = state.influx_report_count.fetch_add(1, Ordering::Relaxed);
            if count % influx_cfg.report_every_n == 0 {
                influx_client.query(point).await?;
            }
        }
    }

    Ok(())
}

fn validate_configs() -> Result<()> {
    let config_path = PathBuf::from(
        std::env::args()
            .nth(2)
            .expect("Second argument (config path) not passed"),
    );
    config::load_all_guild_configs(&config_path)?;
    println!("All guild configs are valid");
    Ok(())
}

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();
    dotenv::dotenv().ok();

    let validate_config_mode = std::env::args().nth(1) == Some("validate-configs".to_owned());

    if validate_config_mode {
        validate_configs()?;
        return Ok(());
    }

    let discord_token = std::env::var("DISCORD_TOKEN")?;

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "chrysanthemum.cfg.yml".to_owned());

    let cfg_json = std::fs::read_to_string(&config_path).expect("couldn't read config file");
    let cfg: Config = serde_yaml::from_str(&cfg_json).expect("Couldn't deserialize config");

    let _sentry_guard = cfg.sentry.as_ref().map(|sentry_config| {
        sentry::init((
            sentry_config.url.clone(),
            sentry::ClientOptions {
                release: sentry::release_name!(),
                traces_sample_rate: sentry_config.sample_rate.unwrap_or(0.01),
                debug: cfg!(debug_assertions),
                ..Default::default()
            },
        ))
    });

    let influx_client = if let Some(influx_cfg) = &cfg.influx {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Token {}", &influx_cfg.token)).unwrap(),
        );
        let reqwest_client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let influx_client = influxdb::Client::new(&influx_cfg.url, &influx_cfg.database)
            .with_http_client(reqwest_client);
        Some(influx_client)
    } else {
        None
    };

    let intents = Intents::GUILD_MESSAGES
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MESSAGE_REACTIONS
        | Intents::MESSAGE_CONTENT;

    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {

    let (shard, mut events) = Shard::builder(discord_token.clone(), intents).build();
    shard.start().await?;

    let http = Arc::new(HttpClient::new(discord_token));
    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE | ResourceType::MEMBER | ResourceType::USER)
        .build();

    let cfg = Arc::new(cfg);
    let spam_history = Arc::new(RwLock::new(filter::SpamHistory::new()));
    let initial_guild_configs =
        config::load_guild_configs(&cfg.guild_config_dir, &cfg.active_guilds)
            .map_err(|(_, e)| e)?;

    let state = State {
        armed: Arc::new(AtomicBool::new(cfg.armed_by_default)),
        http,
        spam_history,
        cfg,
        cache: Arc::new(cache),
        application_id: Arc::new(RwLock::new(None)),
        guild_cfgs: Arc::new(RwLock::new(initial_guild_configs)),
        influx_client: Arc::new(influx_client),
        influx_report_count: Arc::new(AtomicUsize::new(0)),
    };

    tracing::info!("About to enter main event loop; Chrysanthemum is now online.");

    for (guild_id, _) in state.guild_cfgs.read().await.iter() {
        let result = send_notification_to_guild(
            &state,
            *guild_id,
            "Chrysanthemum online",
            "Chrysanthemum is now online.",
        )
        .await;
        if let Err(err) = result {
            tracing::error!(?err, %guild_id, "Error sending up notification");
        }
    }

    let mut interval = tokio::time::interval(Duration::from_secs(
        state.cfg.reload_interval.unwrap_or(DEFAULT_RELOAD_INTERVAL),
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            Some(event) = events.next() => {
                state.cache.update(&event);
                tokio::spawn(handle_event_wrapper(event, state.clone()).instrument(tracing::debug_span!("Handling event")));
            },
            _ = interval.tick() => {
                let result = reload_guild_configs(&state).await;
                if let Err((guild_id, report)) = result {
                    tracing::error!(?guild_id, ?report, "Error reloading guild configuration");
                    send_notification_to_guild(&state, guild_id, "Configuration reload failed", &format!("Failure reason:\n```{:#?}```\nConfiguration changes have **not** been applied.", report)).await?;
                }
            }
        }
    }
    })
}

async fn handle_event_wrapper(event: Event, state: State) {
    let start = Instant::now();
    let result = handle_event(&event, state.clone()).await;
    let end = Instant::now();
    let time = end - start;

    if let Err(report) = result {
        tracing::error!(result = ?report, event = ?event, "Error handling event");
    }

    let (guild_id, channel_id, action_kind) = match event {
        Event::MessageCreate(message) => {
            let message = message.0;

            (
                message.guild_id.unwrap(),
                message.channel_id,
                "message create",
            )
        }
        Event::MessageUpdate(update) => (
            update.guild_id.unwrap(),
            update.channel_id,
            "message update",
        ),
        Event::ReactionAdd(rxn) => {
            let rxn = rxn.0;
            (rxn.guild_id.unwrap(), rxn.channel_id, "reaction")
        }
        _ => return,
    };

    let development = cfg!(debug_assertions);
    let report = EventTimingReport {
        time: Utc::now(),
        time_taken: time.as_secs_f64(),
        guild: guild_id.to_string(),
        channel: channel_id.to_string(),
        action_kind,
        development,
    };

    let result = send_influx_point(&state, &report.into_query("event_report")).await;
    if let Err(err) = result {
        tracing::error!("Unable to send Influx report: {:?}", err);
    }
}

#[tracing::instrument(skip(state))]
async fn handle_event(event: &Event, state: State) -> Result<()> {
    match event {
        Event::MessageCreate(message) => {
            let message = &message.0;
            filter_message(message, state).await?;
        }
        Event::MessageUpdate(update) => {
            filter_message_edit(update, &state).await?;
        }
        Event::ReactionAdd(rxn) => {
            let rxn = &rxn.0;
            filter_reaction(rxn, state).await?;
        }
        Event::Ready(ready) => {
            {
                *state.application_id.write().await = Some(ready.application.id);
            }

            let interaction_http = state.http.interaction(ready.application.id);
            let guild_cfgs = state.guild_cfgs.read().await;

            for (guild_id, guild_config) in guild_cfgs.iter() {
                command::update_guild_commands(
                    &interaction_http,
                    *guild_id,
                    guild_config.slash_commands.as_ref(),
                )
                .await?;
            }
        }
        Event::InteractionCreate(interaction) => {
            let interaction = &interaction.0;
            if let Some(InteractionData::ApplicationCommand(cmd)) = &interaction.data {
                command::handle_command(state.clone(), interaction, cmd.as_ref()).await?;
            }
        }
        _ => {}
    }

    Ok(())
}

#[tracing::instrument(skip(state))]
async fn reload_guild_configs(state: &State) -> Result<(), (Id<GuildMarker>, eyre::Report)> {
    tracing::debug!("Reloading guild configurations");
    let new_guild_configs =
        crate::config::load_guild_configs(&state.cfg.guild_config_dir, &state.cfg.active_guilds)?;
    let mut guild_cfgs = state.guild_cfgs.write().await;
    let application_id = *state.application_id.read().await;

    // We can't interact with commands until we have an application ID from the
    // gateway. Don't try if we don't have one yet.
    if let Some(application_id) = application_id {
        let interaction_http = state.http.interaction(application_id);

        for (guild_id, new_guild_config) in &new_guild_configs {
            tracing::trace!(%guild_id, "Updating guild commands");

            command::update_guild_commands(
                &interaction_http,
                *guild_id,
                new_guild_config.slash_commands.as_ref(),
            )
            .await
            .map_err(|e| (*guild_id, e))?;
        }
    }

    *guild_cfgs = new_guild_configs;

    Ok(())
}

#[tracing::instrument(skip(state))]
async fn filter_message_info<'msg>(
    guild_id: Id<GuildMarker>,
    message_info: &'msg MessageInfo<'_>,
    state: &'msg State,
    context: &'static str,
) -> Result<()> {
    let guild_cfgs = state.guild_cfgs.read().await;
    if let Some(guild_config) = guild_cfgs.get(&guild_id) {
        if message_info.author_is_bot && !guild_config.include_bots {
            tracing::trace!(?guild_id, author = %message_info.author_id, "Skipping message filtration because message was sent by a bot and include_bots is false for this guild");
            return Ok(());
        }

        tracing::trace!(?message_info, "Filtering message");

        if let Some(message_filters) = &guild_config.messages {
            let now = (Utc::now().timestamp_millis() as u64) * 1000;

            let result = crate::message::filter_and_spam_check_message(
                guild_config.spam.as_ref(),
                &message_filters[..],
                guild_config.default_scoping.as_ref(),
                guild_config.default_actions.as_deref(),
                state.spam_history.clone(),
                message_info,
                context,
                now,
            )
            .await;

            if let Err(failure) = result {
                tracing::trace!(%message_info.id, %message_info.channel_id, %message_info.author_id, ?failure, "Message filtered");

                let armed = state.armed.load(Ordering::Relaxed);
                let mut deleted = false;

                for action in failure.actions {
                    tracing::trace!(?action, "Executing action");

                    // We only want to execute Delete actions once per message,
                    // since we'll get a 404 on subsequent requests.
                    if let MessageAction::Delete { .. } = action {
                        if deleted {
                            tracing::trace!(?action, "Skipping duplicate delete action");
                            continue;
                        }

                        deleted = true;
                    }

                    if action.requires_armed() && !armed {
                        tracing::trace!(?action, "Skipping execution because we are not armed");
                        continue;
                    }

                    if let Err(action_err) = action.execute(&state.http).await {
                        tracing::warn!(?action, ?action_err, "Error executing action");
                    }
                }

                tracing::trace!(%message_info.id, %message_info.channel_id, %message_info.author_id, "Filtration completed, all actions executed");

                let report = MessageFilterReport {
                    time: Utc::now(),
                    guild: guild_id.to_string(),
                    channel: message_info.channel_id.to_string(),
                };

                send_influx_point(state, &report.into_query(context)).await?;
                tracing::trace!(%message_info.id, %message_info.channel_id, %message_info.author_id, "Influx point sent");
            }
        }
    }

    Ok(())
}

#[tracing::instrument(skip(state))]
async fn filter_message(message: &Message, state: State) -> Result<()> {
    let guild_id = match message.guild_id {
        Some(id) => id,
        None => return Ok(()),
    };

    let member = match message.member.as_ref() {
        Some(member) => member,
        None => {
            // For non-bot users, this should always be set.
            if !message.author.bot {
                sentry::capture_message(
                    "No `member` field attached to non-bot message",
                    sentry::Level::Error,
                );
                tracing::error!(?message.id, "No `member` field attached to message");
            }

            return Ok(());
        }
    };

    let clean_message_content = crate::message::clean_mentions(&message.content, &message.mentions);

    let message_info = MessageInfo {
        id: message.id,
        author_id: message.author.id,
        channel_id: message.channel_id,
        // We can assume guild_id exists since the DM intent is disabled
        guild_id: message.guild_id.unwrap(),
        timestamp: message.timestamp,
        author_is_bot: message.author.bot,
        author_roles: &member.roles,
        content: &clean_message_content,
        attachments: &message.attachments,
        stickers: &message.sticker_items,
    };

    filter_message_info(guild_id, &message_info, &state, "message create").await
}

#[tracing::instrument(skip(state))]
async fn filter_reaction(rxn: &GatewayReaction, state: State) -> Result<()> {
    if rxn.guild_id.is_none() {
        tracing::trace!("A reaction was added, but no guild ID is present. Ignoring.");
        return Ok(());
    }

    let guild_id = rxn.guild_id.unwrap();

    if rxn.member.is_none() {
        tracing::trace!("A reaction was added, but no member information is present. Ignoring.");
        return Ok(());
    }

    let member = rxn.member.as_ref().unwrap();

    let guild_cfgs = state.guild_cfgs.read().await;
    if let Some(guild_config) = guild_cfgs.get(&guild_id) {
        if member.user.bot && !guild_config.include_bots {
            tracing::trace!("A reaction was added by a bot and include_bots is not set. Ignoring.");
            return Ok(());
        }

        if let Some(reaction_filters) = &guild_config.reactions {
            let reaction_info = ReactionInfo {
                author_is_bot: member.user.bot,
                author_roles: &member.roles,
                author_id: rxn.user_id,
                channel_id: rxn.channel_id,
                message_id: rxn.message_id,
                // We can assume guild_id exists since the DM intent is disabled
                guild_id: rxn.guild_id.unwrap(),
                reaction: rxn.emoji.clone(),
            };

            let filter_result = crate::reaction::filter_reaction(
                reaction_filters,
                guild_config.default_scoping.as_ref(),
                guild_config.default_actions.as_deref(),
                &reaction_info,
            );

            if let Err(failure) = filter_result {
                let armed = state.armed.load(Ordering::Relaxed);
                let mut deleted = false;

                for action in failure.actions {
                    if matches!(action, ReactionAction::Delete { .. }) {
                        if deleted {
                            continue;
                        }

                        deleted = true;
                    }

                    if action.requires_armed() && !armed {
                        continue;
                    }

                    if let Err(action_err) = action.execute(&state.http).await {
                        tracing::warn!(?action_err, ?action, "Error executing reaction action");
                    }
                }

                let report = ReactionFilterReport {
                    time: Utc::now(),
                    guild: guild_id.to_string(),
                    channel: rxn.channel_id.to_string(),
                };

                send_influx_point(&state, &report.into_query("reaction_filter")).await?;
            }
        }
    }

    Ok(())
}

#[tracing::instrument(skip(state))]
async fn filter_message_edit_http(update: &MessageUpdate, state: &State) -> Result<()> {
    let guild_id = match update.guild_id {
        Some(id) => id,
        None => return Ok(()),
    };

    let (author_id, author_is_bot) = match &update.author {
        Some(author) => (author.id, author.bot),
        None => return Ok(()),
    };

    let http_message = state
        .http
        .message(update.channel_id, update.id)
        .await?
        .model()
        .await?;

    let author_roles = {
        let cached_member = state.cache.member(guild_id, author_id);
        match cached_member.as_ref() {
            Some(member) => member.roles().to_owned(),
            None => state
                .http
                .guild_member(guild_id, author_id)
                .await?
                .model()
                .await?
                .roles
                .clone(),
        }
    };

    let message_info = MessageInfo {
        id: http_message.id,
        channel_id: http_message.channel_id,
        // We can assume guild_id exists since the DM intent is disabled
        guild_id: http_message.guild_id.unwrap(),
        timestamp: http_message.timestamp,
        author_roles: &author_roles[..],
        content: &http_message.content,
        attachments: &http_message.attachments,
        stickers: &http_message.sticker_items,
        author_id,
        author_is_bot,
    };

    filter_message_info(guild_id, &message_info, state, "message edit").await
}

#[tracing::instrument(skip(state))]
async fn filter_message_edit(update: &MessageUpdate, state: &State) -> Result<()> {
    let guild_id = match update.guild_id {
        Some(id) => id,
        None => return Ok(()),
    };

    let cached_message = state.cache.message(update.id);

    match (cached_message, update.content.as_deref()) {
        (Some(message), Some(content)) => {
            tracing::trace!("Got message from cache and content from update");

            let (author_id, author_is_bot) = match update.author.as_ref() {
                Some(author) => (author.id, author.bot),
                None => {
                    let cached_author = state.cache.user(message.author());
                    match cached_author {
                        Some(author) => (author.id, author.bot),
                        None => {
                            // Drop the reference to the cached data. In general, updating the
                            // Twilight cache can deadlock when a message gets deleted while
                            // another thread holds a reference to the cached message. Dropping
                            // the cached reference prevents this.
                            drop(message);
                            return filter_message_edit_http(update, state).await;
                        }
                    }
                }
            };

            let timestamp = message.timestamp();
            let attachments = message.attachments().to_owned();
            let sticker_items = message.sticker_items().to_owned();

            // For the same reason as above, we drop the message here.
            drop(message);

            let author_roles = {
                let cached_member = state.cache.member(guild_id, author_id);
                match cached_member.as_ref() {
                    Some(member) => member.roles().to_owned(),
                    None => return filter_message_edit_http(update, state).await,
                }
            };

            let clean_message_content =
                crate::message::clean_mentions(content, update.mentions.as_deref().unwrap_or(&[]));

            let message_info = MessageInfo {
                id: update.id,
                author_id,
                author_is_bot,
                // We can assume guild_id exists since the DM intent is disabled
                guild_id: update.guild_id.unwrap(),
                author_roles: &author_roles[..],
                content: &clean_message_content,
                channel_id: update.channel_id,
                timestamp,
                attachments: &attachments[..],
                stickers: &sticker_items[..],
            };

            filter_message_info(guild_id, &message_info, state, "message edit").await
        }
        _ => filter_message_edit_http(update, state).await,
    }
}

#[tracing::instrument(skip(state))]
async fn send_notification_to_guild(
    state: &State,
    guild_id: Id<GuildMarker>,
    title: &str,
    body: &str,
) -> Result<()> {
    let guild_configs = state.guild_cfgs.read().await;
    if let Some(guild_config) = guild_configs.get(&guild_id) {
        if let Some(notification_config) = &guild_config.notifications {
            let mut builder = EmbedBuilder::new().title(title).description(body);

            if let Some(ping_roles) = &notification_config.ping_roles {
                let mut cc_body = String::new();
                for role in ping_roles {
                    cc_body += &role.mention().to_string();
                    cc_body += " ";
                }

                builder = builder.field(EmbedFieldBuilder::new("CC", cc_body).build());
            }

            state
                .http
                .create_message(notification_config.channel)
                .embeds(&[builder.build()])?
                .await?;
        }
    }

    Ok(())
}
