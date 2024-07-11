use super::super::types::FeedDescriptor;
use super::super::{Watch, Watcher};
use bsky_sdk::api::app::bsky::feed::defs::{
    FeedViewPost, FeedViewPostReasonRefs, PostViewEmbedRefs, ReplyRefParentRefs,
};
use bsky_sdk::api::types::string::Cid;
use bsky_sdk::api::types::Union;
use bsky_sdk::moderation::decision::DecisionContext;
use bsky_sdk::preference::{FeedViewPreference, FeedViewPreferenceData};
use bsky_sdk::Result;
use bsky_sdk::{preference::Preferences, BskyAgent};
use indexmap::IndexMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Mutex};
use tokio::time;

impl Watcher {
    pub fn feed(&self, descriptor: FeedDescriptor) -> impl Watch<Output = Vec<FeedViewPost>> {
        let (tx, _) = broadcast::channel(1);
        FeedWatcher {
            descriptor,
            agent: self.agent.clone(),
            preferences: self.preferences(),
            period: Duration::from_secs(self.config.intervals.feed),
            tx,
            current: Default::default(),
        }
    }
}

pub struct FeedWatcher<W> {
    descriptor: FeedDescriptor,
    agent: Arc<BskyAgent>,
    preferences: W,
    period: Duration,
    tx: broadcast::Sender<()>,
    current: Arc<Mutex<IndexMap<Cid, FeedViewPost>>>,
}

impl<W> Watch for FeedWatcher<W>
where
    W: Watch<Output = Preferences>,
{
    type Output = Vec<FeedViewPost>;

    fn subscribe(&self) -> tokio::sync::watch::Receiver<Self::Output> {
        let (tx, rx) = watch::channel(Default::default());
        let (agent, current, descriptor) = (
            self.agent.clone(),
            self.current.clone(),
            Arc::new(self.descriptor.clone()),
        );
        let (mut preferences, mut quit) = (self.preferences.subscribe(), self.tx.subscribe());
        let mut interval = time::interval(self.period);
        tokio::spawn(async move {
            // skip the first tick
            interval.tick().await;
            loop {
                let tick = interval.tick();
                let (agent, current, descriptor, tx) = (
                    agent.clone(),
                    current.clone(),
                    descriptor.clone(),
                    tx.clone(),
                );
                tokio::select! {
                    changed = preferences.changed() => {
                        if changed.is_ok() {
                            let preferences = preferences.borrow_and_update().clone();
                            tokio::spawn(async move {
                                update(&agent, &current, &descriptor, &preferences, &tx).await;
                            });
                        } else {
                            break log::warn!("preferences channel closed");
                        }
                    }
                    _ = tick => {
                        let preferences = preferences.borrow().clone();
                        tokio::spawn(async move {
                            update(&agent, &current, &descriptor, &preferences, &tx).await;
                        });
                    }
                    _ = quit.recv() => {
                        break;
                    }
                }
            }
            log::debug!("quit");
        });
        rx
    }
    fn unsubscribe(&self) {
        if let Err(e) = self.tx.send(()) {
            log::error!("failed to send quit: {e}");
        }
        self.preferences.unsubscribe();
    }
    fn refresh(&self) {
        self.preferences.refresh();
    }
}

async fn update(
    agent: &BskyAgent,
    current: &Mutex<IndexMap<Cid, FeedViewPost>>,
    descriptor: &FeedDescriptor,
    preferences: &Preferences,
    tx: &watch::Sender<Vec<FeedViewPost>>,
) {
    match calculate_feed(agent, current, descriptor, preferences).await {
        Ok(feed) => {
            tx.send(feed).ok();
        }
        Err(e) => {
            log::error!("failed to get feed view posts: {e}");
        }
    }
}

async fn calculate_feed(
    agent: &BskyAgent,
    current: &Mutex<IndexMap<Cid, FeedViewPost>>,
    descriptor: &FeedDescriptor,
    preferences: &Preferences,
) -> Result<Vec<FeedViewPost>> {
    // TODO: It should not be necessary to get moderator every time unless moderation_prefs has been changed?
    let (moderator, feed) = tokio::join!(agent.moderator(preferences), get_feed(agent, descriptor));
    let moderator = moderator?;
    let mut feed = feed?;
    feed.reverse();
    let mut ret = {
        let mut feed_map = current.lock().await;
        update_feeds(&feed, &mut feed_map);
        feed_map.values().rev().cloned().collect::<Vec<_>>()
    };
    // filter by moderator
    ret.retain(|feed_view_post| {
        let decision = moderator.moderate_post(&feed_view_post.post);
        let ui = decision.ui(DecisionContext::ContentList);
        // TODO: use other results?
        !ui.filter()
    });
    // filter by preferences (following timeline only)
    if matches!(descriptor, FeedDescriptor::Timeline(_)) {
        let pref = if let Some(pref) = preferences.feed_view_prefs.get("home") {
            pref.clone()
        } else {
            FeedViewPreferenceData::default().into()
        };
        ret.retain(|feed_view_post| filter_feed(feed_view_post, &pref));
    }
    Ok(ret)
}

async fn get_feed(agent: &BskyAgent, descriptor: &FeedDescriptor) -> Result<Vec<FeedViewPost>> {
    Ok(match descriptor {
        FeedDescriptor::Feed(generator_view) => {
            agent
                .api
                .app
                .bsky
                .feed
                .get_feed(
                    bsky_sdk::api::app::bsky::feed::get_feed::ParametersData {
                        cursor: None,
                        feed: generator_view.uri.clone(),
                        limit: 30.try_into().ok(),
                    }
                    .into(),
                )
                .await?
                .data
                .feed
        }
        FeedDescriptor::List => Vec::new(),
        FeedDescriptor::Timeline(_) => {
            agent
                .api
                .app
                .bsky
                .feed
                .get_timeline(
                    bsky_sdk::api::app::bsky::feed::get_timeline::ParametersData {
                        algorithm: None,
                        cursor: None,
                        limit: 30.try_into().ok(),
                    }
                    .into(),
                )
                .await?
                .data
                .feed
        }
    })
}

fn update_feeds(feed: &[FeedViewPost], feed_map: &mut IndexMap<Cid, FeedViewPost>) {
    for post in feed {
        if let Some(entry) = feed_map.get_mut(&post.post.cid) {
            // Is the feed view a new repost?
            if match (&entry.reason, &post.reason) {
                (
                    Some(Union::Refs(FeedViewPostReasonRefs::ReasonRepost(curr))),
                    Some(Union::Refs(FeedViewPostReasonRefs::ReasonRepost(next))),
                ) => curr.indexed_at < next.indexed_at,
                (None, Some(_)) => true,
                _ => false,
            } {
                // Remove the old entry
                feed_map.swap_remove(&post.post.cid);
            } else {
                // Just update the post
                entry.post = post.post.clone();
                continue;
            }
        }
        feed_map.insert(post.post.cid.clone(), post.clone());
    }
}

fn filter_feed(feed_view_post: &FeedViewPost, pref: &FeedViewPreference) -> bool {
    // is repost?
    if matches!(
        &feed_view_post.reason,
        Some(Union::Refs(FeedViewPostReasonRefs::ReasonRepost(_)))
    ) {
        return !pref.hide_reposts;
    }
    // is reply?
    if let Some(reply) = &feed_view_post.reply {
        let is_self_reply = matches!(&reply.parent,
            Union::Refs(ReplyRefParentRefs::PostView(post_view))
                if post_view.author.did == feed_view_post.post.author.did
        );
        if pref.hide_replies {
            return is_self_reply;
        }
        if feed_view_post.post.like_count.unwrap_or_default() < pref.hide_replies_by_like_count {
            return is_self_reply;
        }
        if pref.hide_replies_by_unfollowed {
            return matches!(&reply.parent,
                Union::Refs(ReplyRefParentRefs::PostView(parent))
                    if parent.author.viewer.as_ref().map(|viewer| viewer.following.is_some()).unwrap_or_default()
            );
        }
    }
    // is quote post?
    else if matches!(
        &feed_view_post.post.embed,
        Some(Union::Refs(
            PostViewEmbedRefs::AppBskyEmbedRecordView(_)
                | PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(_)
        ))
    ) {
        return !pref.hide_quote_posts;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsky_sdk::api::app::bsky::actor::defs::{ProfileViewBasic, ProfileViewBasicData};
    use bsky_sdk::api::app::bsky::feed::defs::{FeedViewPostData, PostViewData, ReasonRepostData};
    use bsky_sdk::api::records::Record;
    use bsky_sdk::api::types::{string::Datetime, UnknownData};
    use ipld_core::ipld::Ipld;
    use std::collections::BTreeMap;

    fn feed_view_post(cid: Cid, reason_indexed_at: Option<Datetime>) -> FeedViewPost {
        fn profile_view_basic() -> ProfileViewBasic {
            ProfileViewBasicData {
                associated: None,
                avatar: None,
                created_at: None,
                did: "did:fake:post.test".parse().expect("invalid did"),
                display_name: None,
                handle: "post.test".parse().expect("invalid handle"),
                labels: None,
                viewer: None,
            }
            .into()
        }

        FeedViewPostData {
            feed_context: None,
            post: PostViewData {
                author: profile_view_basic(),
                cid,
                embed: None,
                indexed_at: Datetime::now(),
                labels: None,
                like_count: None,
                record: Record::Unknown(UnknownData {
                    r#type: "post".to_string(),
                    data: Ipld::Map(BTreeMap::new()),
                }),
                reply_count: None,
                repost_count: None,
                threadgate: None,
                uri: String::new(),
                viewer: None,
            }
            .into(),
            reason: reason_indexed_at.map(|indexed_at| {
                Union::Refs(FeedViewPostReasonRefs::ReasonRepost(Box::new(
                    ReasonRepostData {
                        by: profile_view_basic(),
                        indexed_at,
                    }
                    .into(),
                )))
            }),
            reply: None,
        }
        .into()
    }

    #[test]
    fn update_feed_views() {
        let cids = [
            "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz2a"
                .parse::<Cid>()
                .expect("invalid cid"),
            "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz3a"
                .parse::<Cid>()
                .expect("invalid cid"),
            "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz4a"
                .parse::<Cid>()
                .expect("invalid cid"),
        ];
        let mut feed_map = IndexMap::new();
        // Empty feeds
        update_feeds(&Vec::new(), &mut feed_map);
        assert_eq!(feed_map.len(), 0);
        // New feed
        update_feeds(&[feed_view_post(cids[0].clone(), None)], &mut feed_map);
        assert_eq!(feed_map.len(), 1);
        // Duplicate feed
        update_feeds(&[feed_view_post(cids[0].clone(), None)], &mut feed_map);
        assert_eq!(feed_map.len(), 1);
        // Duplicated and new feed
        update_feeds(
            &[
                feed_view_post(cids[0].clone(), None),
                feed_view_post(cids[1].clone(), None),
            ],
            &mut feed_map,
        );
        assert_eq!(feed_map.len(), 2);
        assert_eq!(feed_map[0].post.cid, cids[0]);
        assert_eq!(feed_map[1].post.cid, cids[1]);
        // New and duplicated feed
        update_feeds(
            &[
                feed_view_post(cids[2].clone(), None),
                feed_view_post(cids[1].clone(), None),
            ],
            &mut feed_map,
        );
        assert_eq!(feed_map.len(), 3);
        assert_eq!(feed_map[0].post.cid, cids[0]);
        assert_eq!(feed_map[1].post.cid, cids[1]);
        assert_eq!(feed_map[2].post.cid, cids[2]);
        // Duplicated, but updated feed
        update_feeds(
            &[
                feed_view_post(cids[1].clone(), Some(Datetime::now())),
                feed_view_post(cids[2].clone(), None),
            ],
            &mut feed_map,
        );
        assert_eq!(feed_map.len(), 3);
        println!("{:?}", feed_map.keys().collect::<Vec<_>>());
        assert_eq!(feed_map[0].post.cid, cids[0]);
        assert_eq!(feed_map[1].post.cid, cids[2]);
        assert_eq!(feed_map[2].post.cid, cids[1]);
        assert!(feed_map[0].reason.is_none());
        assert!(feed_map[1].reason.is_none());
        assert!(feed_map[2].reason.is_some());
        // Duplicated, but updated feed
        update_feeds(
            &[
                feed_view_post(cids[0].clone(), Some(Datetime::now())),
                feed_view_post(cids[1].clone(), Some(Datetime::now())),
            ],
            &mut feed_map,
        );
        assert_eq!(feed_map.len(), 3);
        println!("{:?}", feed_map.keys().collect::<Vec<_>>());
        assert_eq!(feed_map[1].post.cid, cids[2]);
        assert_eq!(feed_map[0].post.cid, cids[0]);
        assert_eq!(feed_map[2].post.cid, cids[1]);
        assert!(feed_map[0].reason.is_some());
        assert!(feed_map[1].reason.is_none());
        assert!(feed_map[2].reason.is_some());
    }
}
