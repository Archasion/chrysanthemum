use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use twilight_model::channel::ReactionType;
use twilight_model::id::{ChannelId, RoleId, UserId};

use once_cell::sync::OnceCell;
use regex::Regex;
use tokio::sync::RwLock;

use crate::{config, MessageInfo};

static ZALGO_REGEX: OnceCell<Regex> = OnceCell::new();
static INVITE_REGEX: OnceCell<Regex> = OnceCell::new();
static LINK_REGEX: OnceCell<Regex> = OnceCell::new();
static SPOILER_REGEX: OnceCell<Regex> = OnceCell::new();
static EMOJI_REGEX: OnceCell<Regex> = OnceCell::new();
static CUSTOM_EMOJI_REGEX: OnceCell<Regex> = OnceCell::new();
static MENTION_REGEX: OnceCell<Regex> = OnceCell::new();

pub fn init_globals() {
    // The Err case here is if the cell already has a value in it. In this case
    // we want to just ignore it. The only time this will happen is in tests,
    // where each test can call init_globals().
    let _ = ZALGO_REGEX
        .set(Regex::new("\\u0303|\\u035F|\\u034F|\\u0327|\\u031F|\\u0353|\\u032F|\\u0318|\\u0353|\\u0359|\\u0354").unwrap());
    let _ = INVITE_REGEX.set(Regex::new("discord.gg/(\\w+)").unwrap());
    let _ = LINK_REGEX.set(Regex::new("https?://([^/\\s]+)").unwrap());
    let _ = SPOILER_REGEX.set(Regex::new("\\|\\|[^\\|]*\\|\\|").unwrap());
    let _ = EMOJI_REGEX.set(
        Regex::new("\\p{Emoji_Presentation}|\\p{Emoji}\\uFE0F|\\p{Emoji_Modifier_Base}").unwrap(),
    );
    let _ = CUSTOM_EMOJI_REGEX.set(Regex::new("<a?:([^:]+):(\\d+)>").unwrap());
    let _ = MENTION_REGEX.set(Regex::new("<@[!&]?\\d+>").unwrap());
}

pub type FilterResult = Result<(), String>;

fn filter_values<T, V, I>(
    mode: &config::FilterMode,
    context: &str,
    values: &mut I,
    filter_values: &[V],
) -> FilterResult
where
    T: std::fmt::Display,
    V: PartialEq<T>,
    I: Iterator<Item = T>,
{
    let result = match mode {
        config::FilterMode::AllowList => values
            // Note: We use iter().any() instead of contains because we
            // sometimes pass Vec<String> as filter_values, where T is &str -
            // contains isn't smart enough to handle this case.
            .find(|v| !filter_values.iter().any(|f| f == v))
            .map(|v| Err(format!("contains unallowed {} `{}`", context, v))),
        config::FilterMode::DenyList => values
            .find(|v| filter_values.iter().any(|f| f == v))
            .map(|v| Err(format!("contains denied {} `{}`", context, v))),
    };

    result.unwrap_or(Ok(()))
}

impl config::Scoping {
    pub fn is_included(&self, channel: ChannelId, author_roles: &[RoleId]) -> bool {
        if self.include_channels.is_some() {
            if self
                .include_channels
                .as_ref()
                .unwrap()
                .iter()
                .all(|c| *c != channel)
            {
                return false;
            }
        }

        if self.exclude_channels.is_some() {
            if self
                .exclude_channels
                .as_ref()
                .unwrap()
                .iter()
                .any(|c| *c == channel)
            {
                return false;
            }
        }

        if self.exclude_roles.is_some() {
            for excluded_role in self.exclude_roles.as_ref().unwrap() {
                if author_roles.contains(excluded_role) {
                    return false;
                }
            }
        }

        return true;
    }
}

impl config::MessageFilter {
    pub(crate) fn filter_message(&self, message: &MessageInfo<'_>) -> FilterResult {
        self.rules
            .iter()
            .map(|f| f.filter_message(&message))
            .find(|r| r.is_err())
            .unwrap_or(Ok(()))
    }

    pub fn filter_text(&self, text: &str) -> FilterResult {
        self.rules
            .iter()
            .map(|f| f.filter_text(text))
            .find(|r| r.is_err())
            .unwrap_or(Ok(()))
    }
}

impl config::MessageFilterRule {
    pub fn filter_text(&self, text: &str) -> FilterResult {
        match self {
            config::MessageFilterRule::Words { words } => {
                let skeleton = crate::confusable::skeletonize(text);

                tracing::trace!(%text, %skeleton, ?words, "Performing word text filtration");

                if let Some(captures) = words.captures(&skeleton) {
                    Err(format!(
                        "contains word `{}`",
                        captures.get(1).unwrap().as_str()
                    ))
                } else if let Some(captures) = words.captures(&text) {
                    Err(format!(
                        "contains word `{}`",
                        captures.get(1).unwrap().as_str()
                    ))
                } else {
                    Ok(())
                }
            }
            config::MessageFilterRule::Substring { substrings } => {
                let skeleton = crate::confusable::skeletonize(text);

                tracing::trace!(%text, %skeleton, ?substrings, "Performing substring text filtration");

                if let Some(captures) = substrings.captures(&skeleton) {
                    Err(format!(
                        "contains substring `{}`",
                        captures.get(0).unwrap().as_str()
                    ))
                } else if let Some(captures) = substrings.captures(&text) {
                    Err(format!(
                        "contains substring `{}`",
                        captures.get(0).unwrap().as_str()
                    ))
                } else {
                    Ok(())
                }
            }
            config::MessageFilterRule::Regex { regexes } => {
                let skeleton = crate::confusable::skeletonize(text);

                tracing::trace!(%text, %skeleton, ?regexes, "Performing regex text filtration");

                for regex in regexes {
                    if regex.is_match(&skeleton) || regex.is_match(&text) {
                        return Err(format!("matches regex `{}`", regex));
                    }
                }

                Ok(())
            }
            config::MessageFilterRule::Zalgo => {
                let zalgo_regex = ZALGO_REGEX.get().unwrap();
                if zalgo_regex.is_match(text) {
                    Err("contains zalgo".to_owned())
                } else {
                    Ok(())
                }
            }
            config::MessageFilterRule::Invite { mode, invites } => {
                let invite_regex = INVITE_REGEX.get().unwrap();
                let mut invite_ids = invite_regex
                    .captures_iter(text)
                    .map(|c| c.get(1).unwrap().as_str());
                filter_values(mode, "invite", &mut invite_ids, invites)
            }
            config::MessageFilterRule::Link { mode, domains } => {
                let link_regex = LINK_REGEX.get().unwrap();
                let mut link_domains = link_regex
                    .captures_iter(text)
                    .map(|c| c.get(1).unwrap().as_str())
                    // Invites should be handled separately.
                    .filter(|v| (*v) != "discord.gg");

                let result = match mode {
                    config::FilterMode::AllowList => link_domains
                        // Hack (#12): Treat www.domain.xyz as domain.xyz.
                        .find(|v| !domains.iter().any(|f| f == v || v == &format!("www.{}", f)))
                        .map(|v| Err(format!("contains unallowed domain `{}`", v))),
                    config::FilterMode::DenyList => link_domains
                        .find(|v| domains.iter().any(|f| f == v || v == &format!("www.{}", f)))
                        .map(|v| Err(format!("contains denied domain `{}`", v))),
                };

                result.unwrap_or(Ok(()))
            }
            config::MessageFilterRule::EmojiName { names } => {
                for capture in CUSTOM_EMOJI_REGEX.get().unwrap().captures_iter(text) {
                    let name = capture.get(1).unwrap().as_str();
                    let substring_match = names.captures(name);
                    if let Some(substring_match) = substring_match {
                        return Err(format!(
                            "contains emoji with denied name substring {}",
                            substring_match.get(0).unwrap().as_str()
                        ));
                    }
                }

                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn filter_message(&self, message: &MessageInfo<'_>) -> FilterResult {
        match self {
            config::MessageFilterRule::MimeType {
                mode,
                types,
                allow_unknown,
            } => {
                if message.attachments.iter().any(|a| a.content_type.is_none()) && !allow_unknown {
                    return Err("unknown content type for attachment".to_owned());
                }

                let mut attachment_types = message
                    .attachments
                    .iter()
                    .filter_map(|a| a.content_type.as_deref());
                filter_values(mode, "content type", &mut attachment_types, types)
            }
            config::MessageFilterRule::StickerId { mode, stickers } => filter_values(
                mode,
                "sticker",
                &mut message.stickers.iter().map(|s| s.id),
                stickers,
            ),
            config::MessageFilterRule::StickerName { stickers } => {
                for sticker in message.stickers.iter() {
                    let substring_match = stickers.captures_iter(&sticker.name).nth(0);
                    if let Some(substring_match) = substring_match {
                        return Err(format!(
                            "contains sticker with denied name substring `{}`",
                            substring_match.get(0).unwrap().as_str()
                        ));
                    }
                }

                Ok(())
            }
            _ => self.filter_text(&message.content),
        }
    }
}

impl config::ReactionFilter {
    pub fn filter_reaction(&self, reaction: &ReactionType) -> FilterResult {
        self.rules
            .iter()
            .map(|f| f.filter_reaction(&reaction))
            .find(|r| r.is_err())
            .unwrap_or(Ok(()))
    }
}

impl config::ReactionFilterRule {
    pub fn filter_reaction(&self, reaction: &ReactionType) -> FilterResult {
        match self {
            config::ReactionFilterRule::Default {
                emoji: filtered_emoji,
                mode,
            } => {
                if let ReactionType::Unicode { name } = reaction {
                    match mode {
                        config::FilterMode::AllowList => {
                            if !filtered_emoji.contains(&name) {
                                Err(format!("reacted with unallowed emoji {}", name))
                            } else {
                                Ok(())
                            }
                        }
                        config::FilterMode::DenyList => {
                            if filtered_emoji.contains(&name) {
                                Err(format!("reacted with denied emoji {}", name))
                            } else {
                                Ok(())
                            }
                        }
                    }
                } else {
                    Ok(())
                }
            }
            config::ReactionFilterRule::CustomId {
                emoji: filtered_emoji,
                mode,
            } => {
                if let ReactionType::Custom { id, .. } = reaction {
                    match mode {
                        config::FilterMode::AllowList => {
                            if !filtered_emoji.contains(&id) {
                                Err(format!("reacted with unallowed emoji {}", id))
                            } else {
                                Ok(())
                            }
                        }
                        config::FilterMode::DenyList => {
                            if filtered_emoji.contains(&id) {
                                Err(format!("reacted with denied emoji {}", id))
                            } else {
                                Ok(())
                            }
                        }
                    }
                } else {
                    Ok(())
                }
            }
            config::ReactionFilterRule::CustomName { names } => {
                if let ReactionType::Custom {
                    name: Some(name), ..
                } = reaction
                {
                    if names.is_match(&name) {
                        Err(format!("reacted with denied emoji name {}", name))
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct SpamRecord {
    content: String,
    emoji: u8,
    links: u8,
    attachments: u8,
    spoilers: u8,
    mentions: u8,
    sent_at: u64,
}

impl SpamRecord {
    pub(crate) fn from_message(message: &MessageInfo) -> SpamRecord {
        let spoilers = SPOILER_REGEX
            .get()
            .unwrap()
            .find_iter(&message.content)
            .count();
        let emoji = EMOJI_REGEX
            .get()
            .unwrap()
            .find_iter(&message.content)
            .count();
        let links = LINK_REGEX
            .get()
            .unwrap()
            .find_iter(&message.content)
            .count();
        let mentions = MENTION_REGEX
            .get()
            .unwrap()
            .find_iter(&message.content)
            .count();

        SpamRecord {
            // Unfortunately, this clone is necessary, because `message` will be
            // dropped while we still need this.
            content: message.content.to_string(),
            emoji: emoji as u8,
            links: links as u8,
            // `as` cast is safe for our purposes. If the message has more than
            // 255 attachments, `as` will give us a u8 with a value of 255.
            attachments: message.attachments.len() as u8,
            spoilers: spoilers as u8,
            mentions: mentions as u8,
            sent_at: message.timestamp.as_micros(),
        }
    }
}

pub type SpamHistory = HashMap<UserId, Arc<Mutex<VecDeque<SpamRecord>>>>;

fn exceeds_spam_thresholds(
    history: &VecDeque<SpamRecord>,
    current_record: &SpamRecord,
    config: &config::SpamFilter,
) -> FilterResult {
    let (emoji_sum, link_sum, attachment_sum, spoiler_sum, mention_sum, matching_duplicates) =
        history
            .iter()
            // Start with a value of 1 for matching_duplicates because the current spam record
            // is always a duplicate of itself.
            .fold(
                (
                    current_record.emoji,
                    current_record.links,
                    current_record.attachments,
                    current_record.spoilers,
                    current_record.mentions,
                    1u8,
                ),
                |(
                    total_emoji,
                    total_links,
                    total_attachments,
                    total_spoilers,
                    total_mentions,
                    total_duplicates,
                ),
                 record| {
                    (
                        total_emoji.saturating_add(record.emoji),
                        total_links.saturating_add(record.links),
                        total_attachments.saturating_add(record.attachments),
                        total_spoilers.saturating_add(record.spoilers),
                        total_mentions.saturating_add(record.mentions),
                        total_duplicates
                            .saturating_add((record.content == current_record.content) as u8),
                    )
                },
            );

    tracing::trace!(
        "Spam summary: {} emoji, {} links, {} attachments, {} spoilers, {} mentions, {} duplicates",
        emoji_sum,
        link_sum,
        attachment_sum,
        spoiler_sum,
        mention_sum,
        matching_duplicates
    );

    if config.emoji.is_some() && emoji_sum > config.emoji.unwrap() && current_record.emoji > 0 {
        Err("sent too many emoji".to_owned())
    } else if config.links.is_some() && link_sum > config.links.unwrap() && current_record.links > 0
    {
        Err("sent too many links".to_owned())
    } else if config.attachments.is_some()
        && attachment_sum > config.attachments.unwrap()
        && current_record.attachments > 0
    {
        Err("sent too many attachments".to_owned())
    } else if config.spoilers.is_some()
        && spoiler_sum > config.spoilers.unwrap()
        && current_record.spoilers > 0
    {
        Err("sent too many spoilers".to_owned())
    } else if config.mentions.is_some()
        && mention_sum > config.mentions.unwrap()
        && current_record.mentions > 0
    {
        Err("sent too many mentions".to_owned())
    } else if config.duplicates.is_some() && matching_duplicates > config.duplicates.unwrap() {
        Err("sent too many duplicate messages".to_owned())
    } else {
        Ok(())
    }
}

pub(crate) async fn check_spam_record(
    message: &MessageInfo<'_>,
    config: &config::SpamFilter,
    spam_history: Arc<RwLock<SpamHistory>>,
    now: u64,
) -> FilterResult {
    let new_spam_record = SpamRecord::from_message(&message);
    let author_spam_history = {
        let read_history = spam_history.read().await;
        // This is tricky: We need to release the read lock, acquire a write lock, and
        // then insert the new history entry into the map.
        if !read_history.contains_key(&message.author_id) {
            drop(read_history);

            let new_history = Arc::new(Mutex::new(VecDeque::new()));
            let mut write_history = spam_history.write().await;
            write_history.insert(message.author_id, new_history.clone());
            new_history
        } else {
            read_history.get(&message.author_id).unwrap().clone()
        }
    };

    let mut spam_history = author_spam_history.lock().unwrap();

    let mut cleared_count = 0;
    loop {
        match spam_history.front() {
            Some(front) => {
                if now.saturating_sub(front.sent_at) > (config.interval as u64) * 1_000_000 {
                    spam_history.pop_front();
                    cleared_count += 1;
                } else {
                    break;
                }
            }
            None => break,
        }
    }

    tracing::trace!(
        "Cleared {} spam records for user {}",
        cleared_count,
        message.author_id
    );

    let result = exceeds_spam_thresholds(&spam_history, &new_spam_record, &config);
    spam_history.push_back(new_spam_record);
    result
}

#[cfg(test)]
mod test {
    mod scoping {
        use pretty_assertions::assert_eq;
        use twilight_model::id::{RoleId, ChannelId};

        use crate::config::Scoping;

        const EMPTY_ROLES: &'static [RoleId] = &[];

        #[test]
        fn include_channels() {
            let scoping = Scoping {
                exclude_channels: None,
                exclude_roles: None,
                include_channels: Some(vec![
                    ChannelId::new(1).unwrap(),
                ]),
            };

            assert_eq!(scoping.is_included(ChannelId::new(2).unwrap(), EMPTY_ROLES), false);
            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), EMPTY_ROLES), true);
        }

        #[test]
        fn exclude_channels() {
            let scoping = Scoping {
                include_channels: None,
                exclude_roles: None,
                exclude_channels: Some(vec![
                    ChannelId::new(1).unwrap(),
                ]),
            };

            assert_eq!(scoping.is_included(ChannelId::new(2).unwrap(), EMPTY_ROLES), true);
            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), EMPTY_ROLES), false);
        }

        #[test]
        fn exclude_roles() {
            let scoping = Scoping {
                include_channels: None,
                exclude_roles: Some(vec![
                    RoleId::new(1).unwrap(),
                ]),
                exclude_channels: None,
            };

            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), EMPTY_ROLES), true);
            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), &[RoleId::new(1).unwrap()]), false);
            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), &[RoleId::new(2).unwrap()]), true);
        }

        #[test]
        fn complex_scoping() {
            let scoping = Scoping {
                include_channels: Some(vec![
                    ChannelId::new(1).unwrap(),
                ]),
                exclude_channels: None,
                exclude_roles: Some(vec![
                    RoleId::new(1).unwrap(),
                ])
            };

            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), EMPTY_ROLES), true);
            assert_eq!(scoping.is_included(ChannelId::new(2).unwrap(), EMPTY_ROLES), false);
            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), &[RoleId::new(1).unwrap()]), false);
            assert_eq!(scoping.is_included(ChannelId::new(2).unwrap(), &[RoleId::new(1).unwrap()]), false);
            assert_eq!(scoping.is_included(ChannelId::new(1).unwrap(), &[RoleId::new(2).unwrap()]), true);
            assert_eq!(scoping.is_included(ChannelId::new(2).unwrap(), &[RoleId::new(2).unwrap()]), false);
        }
    }

    mod messages {
        use pretty_assertions::assert_eq;

        use regex::Regex;
        use twilight_model::{id::{AttachmentId}, channel::{Attachment, message::sticker::{MessageSticker, StickerId}}};

        use crate::config::{MessageFilterRule, FilterMode};
        use crate::model::test::{message, GOOD_CONTENT, BAD_CONTENT};

        #[test]
        fn filter_words() {
            let rule = MessageFilterRule::Words {
                words: Regex::new("\\b(bad|asdf)\\b").unwrap(),
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains word `asdf`".to_owned()));
        }

        #[test]
        fn filter_substrings() {
            let rule = MessageFilterRule::Substring {
                substrings: Regex::new("(bad|asdf)").unwrap(),
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains substring `asdf`".to_owned()))
        }

        #[test]
        fn filter_regex() {
            let rule = MessageFilterRule::Regex {
                regexes: vec![
                    Regex::new("sd").unwrap(),
                ],
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("matches regex `sd`".to_owned()));
        }

        #[test]
        fn filter_zalgo() {
            super::super::init_globals();

            let rule = MessageFilterRule::Zalgo;

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains zalgo".to_owned()));
        }

        #[test]
        fn filter_mimetype_deny() {
            let rule = MessageFilterRule::MimeType {
                mode: FilterMode::DenyList,
                types: vec!["image/png".to_owned()],
                allow_unknown: false,
            };

            let mut ok_message = message(GOOD_CONTENT);
            let ok_attachments = [
                Attachment {
                    content_type: Some("image/jpg".to_owned()),
                    ephemeral: false,
                    filename: "file".to_owned(),
                    description: None,
                    height: None,
                    id: AttachmentId::new(1).unwrap(),
                    proxy_url: "doesn't_matter".to_owned(),
                    size: 1,
                    url: "doesn't_matter".to_owned(),
                    width: None,
                }
            ];
            ok_message.attachments = &ok_attachments;

            let mut wrong_message = message(BAD_CONTENT);
            let wrong_attachments = [Attachment {
                content_type: Some("image/png".to_owned()),
                ephemeral: false,
                filename: "file".to_owned(),
                description: None,
                height: None,
                id: AttachmentId::new(1).unwrap(),
                proxy_url: "doesn't_matter".to_owned(),
                size: 1,
                url: "doesn't_matter".to_owned(),
                width: None,
            }];
            wrong_message.attachments = &wrong_attachments;

            let mut missing_content_type_message = message(BAD_CONTENT);
            let missing_content_type_attachments = [Attachment {
                content_type: None,
                ephemeral: false,
                filename: "file".to_owned(),
                description: None,
                height: None,
                id: AttachmentId::new(1).unwrap(),
                proxy_url: "doesn't_matter".to_owned(),
                size: 1,
                url: "doesn't_matter".to_owned(),
                width: None,
            }];
            missing_content_type_message.attachments = &missing_content_type_attachments;

            assert_eq!(rule.filter_message(&ok_message), Ok(()));
            assert_eq!(rule.filter_message(&wrong_message), Err("contains denied content type `image/png`".to_owned()));
            assert_eq!(rule.filter_message(&missing_content_type_message), Err("unknown content type for attachment".to_owned()));
        }

        #[test]
        fn filter_mimetype_allow() {
            let rule = MessageFilterRule::MimeType {
                mode: FilterMode::AllowList,
                types: vec!["image/png".to_owned()],
                allow_unknown: false,
            };

            let mut ok_message = message(GOOD_CONTENT);
            let ok_attachments = [
                Attachment {
                    content_type: Some("image/png".to_owned()),
                    ephemeral: false,
                    filename: "file".to_owned(),
                    description: None,
                    height: None,
                    id: AttachmentId::new(1).unwrap(),
                    proxy_url: "doesn't_matter".to_owned(),
                    size: 1,
                    url: "doesn't_matter".to_owned(),
                    width: None,
                }
            ];
            ok_message.attachments = &ok_attachments;

            let mut wrong_message = message(BAD_CONTENT);
            let wrong_attachments = [Attachment {
                content_type: Some("image/jpg".to_owned()),
                ephemeral: false,
                filename: "file".to_owned(),
                description: None,
                height: None,
                id: AttachmentId::new(1).unwrap(),
                proxy_url: "doesn't_matter".to_owned(),
                size: 1,
                url: "doesn't_matter".to_owned(),
                width: None,
            }];
            wrong_message.attachments = &wrong_attachments;

            let mut missing_content_type_message = message(BAD_CONTENT);
            let missing_content_type_attachments = [Attachment {
                content_type: None,
                ephemeral: false,
                filename: "file".to_owned(),
                description: None,
                height: None,
                id: AttachmentId::new(1).unwrap(),
                proxy_url: "doesn't_matter".to_owned(),
                size: 1,
                url: "doesn't_matter".to_owned(),
                width: None,
            }];
            missing_content_type_message.attachments = &missing_content_type_attachments;

            assert_eq!(rule.filter_message(&ok_message), Ok(()));
            assert_eq!(rule.filter_message(&wrong_message), Err("contains unallowed content type `image/jpg`".to_owned()));
            assert_eq!(rule.filter_message(&missing_content_type_message), Err("unknown content type for attachment".to_owned()));
        }

        #[test]
        fn filter_domain_deny() {
            super::super::init_globals();

            let rule = MessageFilterRule::Link {
                mode: FilterMode::DenyList,
                domains: vec!["example.com".to_owned()],
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains denied domain `example.com`".to_owned()));
        }

        #[test]
        fn filter_domain_allow() {
            super::super::init_globals();

            let rule = MessageFilterRule::Link {
                mode: FilterMode::AllowList,
                domains: vec!["discord.gg".to_owned()],
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains unallowed domain `example.com`".to_owned()));
        }

        #[test]
        fn filter_invite_deny() {
            super::super::init_globals();

            let rule = MessageFilterRule::Invite {
                mode: FilterMode::DenyList,
                invites: vec!["evilserver".to_owned()],
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains denied invite `evilserver`".to_owned()));
        }

        #[test]
        fn filter_invite_allow() {
            super::super::init_globals();

            let rule = MessageFilterRule::Invite {
                mode: FilterMode::AllowList,
                invites: vec!["roblox".to_owned()],
            };

            assert_eq!(rule.filter_message(&message(GOOD_CONTENT)), Ok(()));
            assert_eq!(rule.filter_message(&message(BAD_CONTENT)), Err("contains unallowed invite `evilserver`".to_owned()));
        }

        #[test]
        fn filter_sticker_name() {
            let rule = MessageFilterRule::StickerName {
                stickers: Regex::new("(badsticker)").unwrap(),
            };

            let mut good_message = message(GOOD_CONTENT);
            let good_stickers = [MessageSticker {
                format_type: twilight_model::channel::message::sticker::StickerFormatType::Apng,
                id: StickerId::new(1).unwrap(),
                name: "goodsticker".to_owned(),
            }];
            good_message.stickers = &good_stickers;

            let mut bad_message = message(BAD_CONTENT);
            let bad_stickers = [MessageSticker {
                format_type: twilight_model::channel::message::sticker::StickerFormatType::Apng,
                id: StickerId::new(1).unwrap(),
                name: "badsticker".to_owned(),
            }];
            bad_message.stickers = &bad_stickers;

            assert_eq!(rule.filter_message(&good_message), Ok(()));
            assert_eq!(rule.filter_message(&bad_message), Err("contains sticker with denied name substring `badsticker`".to_owned()));
        }

        #[test]
        fn filter_sticker_id_allow() {
            let rule = MessageFilterRule::StickerId {
                mode: FilterMode::AllowList,
                stickers: vec![StickerId::new(1).unwrap()],
            };

            let mut good_message = message(GOOD_CONTENT);
            let good_stickers = [MessageSticker {
                format_type: twilight_model::channel::message::sticker::StickerFormatType::Apng,
                id: StickerId::new(1).unwrap(),
                name: "goodsticker".to_owned(),
            }];
            good_message.stickers = &good_stickers;

            let mut bad_message = message(BAD_CONTENT);
            let bad_stickers = [MessageSticker {
                format_type: twilight_model::channel::message::sticker::StickerFormatType::Apng,
                id: StickerId::new(2).unwrap(),
                name: "badsticker".to_owned(),
            }];
            bad_message.stickers = &bad_stickers;

            assert_eq!(rule.filter_message(&good_message), Ok(()));
            assert_eq!(rule.filter_message(&bad_message), Err("contains unallowed sticker `2`".to_owned()));
        }

        #[test]
        fn filter_sticker_id_deny() {
            let rule = MessageFilterRule::StickerId {
                mode: FilterMode::DenyList,
                stickers: vec![StickerId::new(2).unwrap()],
            };

            let mut good_message = message(GOOD_CONTENT);
            let good_stickers = [MessageSticker {
                format_type: twilight_model::channel::message::sticker::StickerFormatType::Apng,
                id: StickerId::new(1).unwrap(),
                name: "goodsticker".to_owned(),
            }];
            good_message.stickers = &good_stickers;

            let mut bad_message = message(BAD_CONTENT);
            let bad_stickers = [MessageSticker {
                format_type: twilight_model::channel::message::sticker::StickerFormatType::Apng,
                id: StickerId::new(2).unwrap(),
                name: "badsticker".to_owned(),
            }];
            bad_message.stickers = &bad_stickers;

            assert_eq!(rule.filter_message(&good_message), Ok(()));
            assert_eq!(rule.filter_message(&bad_message), Err("contains denied sticker `2`".to_owned()));
        }
    
        #[test]
        fn filter_words_with_skeletonization() {
            let rule = MessageFilterRule::Words {
                words: Regex::new("\\b(bad)\\b").unwrap(),
            };

            assert_eq!(rule.filter_message(&message("b⍺d message")), Err("contains word `bad`".to_owned()));
        }

        #[test]
        fn filter_substrings_with_skeletonization() {
            let rule = MessageFilterRule::Substring {
                substrings: Regex::new("(bad)").unwrap(),
            };

            assert_eq!(rule.filter_message(&message("b⍺dmessage")), Err("contains substring `bad`".to_owned()));
        }

        #[test]
        fn filter_regex_with_skeletonization() {
            let rule = MessageFilterRule::Regex {
                regexes: vec![Regex::new("bad").unwrap()],
            };

            assert_eq!(rule.filter_message(&message("b⍺dmessage")), Err("matches regex `bad`".to_owned()));
        }
    }

    mod spam {
        use std::{num::NonZeroU64, collections::{VecDeque, HashMap}, sync::Arc};

        use pretty_assertions::assert_eq;

        use tokio::sync::RwLock;
        use twilight_model::{id::{MessageId, UserId, ChannelId, AttachmentId}, datetime::Timestamp, channel::Attachment};

        use crate::{model::MessageInfo, filter::{SpamRecord, exceeds_spam_thresholds, init_globals}, config::SpamFilter};

        use crate::model::test::{message_at_time, GOOD_CONTENT};

        #[test]
        fn spam_record_creation() {
            super::super::init_globals();

            let mut info = MessageInfo {
                author_is_bot: false,
                id: MessageId(NonZeroU64::new(1).unwrap()),
                author_id: UserId(NonZeroU64::new(1).unwrap()),
                channel_id: ChannelId(NonZeroU64::new(1).unwrap()),
                author_roles: &[],
                content: "test message https://discord.gg/ ||spoiler|| 💟 <@123>",
                timestamp: Timestamp::from_secs(100).unwrap(),
                attachments: &[],
                stickers: &[],
            };

            let attachments = [
                Attachment {
                    content_type: Some("image/jpg".to_owned()),
                    ephemeral: false,
                    filename: "file".to_owned(),
                    description: None,
                    height: None,
                    id: AttachmentId::new(1).unwrap(),
                    proxy_url: "doesn't_matter".to_owned(),
                    size: 1,
                    url: "doesn't_matter".to_owned(),
                    width: None,
                }
            ];
            info.attachments = &attachments;

            let record = SpamRecord::from_message(&info);
            assert_eq!(record.content, info.content);
            assert_eq!(record.spoilers, 1);
            assert_eq!(record.emoji, 1);
            assert_eq!(record.links, 1);
            assert_eq!(record.mentions, 1);
            assert_eq!(record.attachments, 1);
            assert_eq!(record.sent_at, 100_000_000);
        }

        fn setup_for_testing() -> (VecDeque<SpamRecord>, SpamFilter) {
            let mut history = VecDeque::new();
            let config = SpamFilter {
                emoji: Some(2),
                duplicates: Some(1),
                links: Some(2),
                attachments: Some(2),
                spoilers: Some(2),
                mentions: Some(2),
                interval: 30,
                actions: None,
                scoping: None,
            };

            let initial_record = SpamRecord {
                content: "asdf".to_owned(),
                spoilers: 1,
                emoji: 1,
                links: 1,
                mentions: 1,
                attachments: 1,
                sent_at: 0,
            };

            history.push_back(initial_record);

            (history, config)
        }

        #[test]
        fn spam_checker_noop() {
            let (history, config) = setup_for_testing();

            let succeeding_record = SpamRecord {
                content: "not asdf".to_owned(),
                spoilers: 0,
                emoji: 0,
                links: 0,
                mentions: 0,
                attachments: 0,
                sent_at: 10,
            };

            let result = exceeds_spam_thresholds(&history, &succeeding_record, &config);
            assert_eq!(result, Ok(()))
        }

        #[test]
        fn content_spam_checker() {
            let (history, config) = setup_for_testing();

            let failing_record = SpamRecord {
                content: "asdf".to_owned(),
                spoilers: 0,
                emoji: 0,
                links: 0,
                mentions: 0,
                attachments: 0,
                sent_at: 10,
            };

            let result = exceeds_spam_thresholds(&history, &failing_record, &config);
            assert_eq!(result, Err("sent too many duplicate messages".to_owned()));
        }

        #[test]
        fn emoji_spam_checker() {
            let (history, config) = setup_for_testing();

            let failing_record = SpamRecord {
                content: "foo".to_owned(),
                spoilers: 0,
                emoji: 2,
                links: 0,
                mentions: 0,
                attachments: 0,
                sent_at: 10,
            };

            let result = exceeds_spam_thresholds(&history, &failing_record, &config);
            assert_eq!(result, Err("sent too many emoji".to_owned()));
        }

        #[test]
        fn link_spam_checker() {
            let (history, config) = setup_for_testing();

            let failing_record = SpamRecord {
                content: "foo".to_owned(),
                spoilers: 0,
                emoji: 0,
                links: 2,
                mentions: 0,
                attachments: 0,
                sent_at: 10,
            };

            let result = exceeds_spam_thresholds(&history, &failing_record, &config);
            assert_eq!(result, Err("sent too many links".to_owned()));
        }

        #[test]
        fn mention_spam_checker() {
            let (history, config) = setup_for_testing();

            let failing_record = SpamRecord {
                content: "foo".to_owned(),
                spoilers: 0,
                emoji: 0,
                links: 0,
                mentions: 2,
                attachments: 0,
                sent_at: 10,
            };

            let result = exceeds_spam_thresholds(&history, &failing_record, &config);
            assert_eq!(result, Err("sent too many mentions".to_owned()));
        }

        #[test]
        fn attachment_spam_checker() {
            let (history, config) = setup_for_testing();

            let failing_record = SpamRecord {
                content: "foo".to_owned(),
                spoilers: 0,
                emoji: 0,
                links: 0,
                mentions: 0,
                attachments: 2,
                sent_at: 10,
            };

            let result = exceeds_spam_thresholds(&history, &failing_record, &config);
            assert_eq!(result, Err("sent too many attachments".to_owned()));
        }
    
        #[tokio::test]
        async fn remove_old_records() {
            init_globals();

            let history = HashMap::new();

            let config = SpamFilter {
                emoji: None,
                duplicates: Some(1),
                links: None,
                attachments: None,
                spoilers: None,
                mentions: None,
                interval: 30,
                actions: None,
                scoping: None,
            };

            let history = Arc::new(RwLock::new(history));

            let first_message = message_at_time(GOOD_CONTENT, 5);
            let result = super::super::check_spam_record(&first_message, &config, history.clone(), 10 * 1_000_000).await;
            assert_eq!(result, Ok(()));

            let second_message = message_at_time(GOOD_CONTENT, 15);
            let result = super::super::check_spam_record(&second_message, &config, history.clone(), 20 * 1_000_000).await;
            assert_eq!(result, Err("sent too many duplicate messages".to_owned()));

            let third_message = message_at_time(GOOD_CONTENT, 45);
            let result = super::super::check_spam_record(&third_message, &config, history.clone(), 60 * 1_000_000).await;
            assert_eq!(result, Ok(()));

            let read_history = history.read().await;
            let read_history_queue = read_history.get(&crate::model::test::USER_ID).expect("user ID not in spam record?").lock().expect("couldn't lock mutex");
            assert_eq!(read_history_queue.len(), 1);
        }
    }
}
