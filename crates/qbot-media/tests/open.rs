#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use qbot_agent::{ChatView, RunState, Tool, ToolCx, ToolError, Trigger};
use qbot_context::{ChatLine, MemberStanding, Part, Speaker};
use qbot_core::{AccountId, GroupId, MemberNo, MessageId, RunId, SystemClock, UnixMillis};
use qbot_llm::MediaStore;
use qbot_media::{
    ArchivedMedia, FetchError, Fetcher, Kind, MediaRef, MediaRefs, OpenImages, OpenImagesArgs,
    PictureArg, RefsError,
};

fn group() -> GroupId {
    GroupId::new(9).unwrap()
}

struct Refs(HashMap<(i64, &'static str, u32), MediaRef>);

#[async_trait]
impl MediaRefs for Refs {
    async fn media_ref(
        &self,
        _: GroupId,
        message: MessageId,
        kind: Kind,
        index: u32,
    ) -> Result<Option<MediaRef>, RefsError> {
        Ok(self.0.get(&(message.get(), kind.marker(), index)).cloned())
    }
}

#[derive(Default)]
struct Fetch {
    calls: AtomicUsize,
    gone: Mutex<Vec<String>>,
}

#[async_trait]
impl Fetcher for Fetch {
    async fn image(&self, reference: &MediaRef, _: u64) -> Result<Vec<u8>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let key = reference.key.clone().unwrap();
        if self.gone.lock().unwrap().contains(&key) {
            return Err(FetchError::Unreadable);
        }
        let mut bytes = b"\x89PNG ".to_vec();
        bytes.extend_from_slice(key.as_bytes());
        Ok(bytes)
    }
    async fn voice(&self, _: &MediaRef, _: u64) -> Result<Vec<u8>, FetchError> {
        Err(FetchError::Unreadable)
    }
}

fn reference(key: &str) -> MediaRef {
    MediaRef {
        key: Some(key.into()),
        ..MediaRef::default()
    }
}

fn line(id: i64) -> ChatLine {
    ChatLine {
        message: MessageId::new(id).unwrap(),
        speaker: Speaker::Member {
            account: AccountId::new(5).unwrap(),
            number: MemberNo::new(1),
            standing: MemberStanding::Normal,
        },
        at: UnixMillis::new(0),
        text: "[image] [image] [sticker]".into(),
    }
}

fn pic(message: i64, position: u32, sticker: bool) -> PictureArg {
    PictureArg {
        message,
        position,
        sticker,
    }
}

#[tokio::test]
async fn pictures_open_by_message_and_position_and_are_fetched_once() {
    let refs = Refs(HashMap::from([
        ((10, "image", 0), reference("a")),
        ((10, "image", 1), reference("b")),
        ((10, "sticker", 0), reference("s")),
    ]));
    let fetch = Arc::new(Fetch::default());
    fetch.gone.lock().unwrap().push("b".into());
    let store = Arc::new(ArchivedMedia::new(Arc::new(refs), fetch.clone(), 1000));
    let tool = OpenImages(store.clone());
    let mut view = ChatView::default();
    view.absorb(&[line(10)]);
    let state = RunState::default();
    let trigger = Trigger::Addressed {
        message: MessageId::new(10).unwrap(),
        sender: AccountId::new(5).unwrap(),
    };
    let cx = ToolCx {
        group: group(),
        run: RunId::new(1),
        trigger: &trigger,
        view: &view,
        state: &state,
        clock: &SystemClock,
    };

    let out = tool
        .call(
            &cx,
            OpenImagesArgs {
                pictures: vec![
                    pic(10, 1, false),
                    pic(10, 2, false),
                    pic(10, 1, true),
                    pic(10, 3, false),
                ],
            },
        )
        .await
        .unwrap();
    let texts: Vec<String> = out
        .content
        .iter()
        .map(|p| match p {
            Part::Text(t) => t.clone(),
            Part::Image { key } => format!("<{key}>"),
        })
        .collect();
    assert_eq!(
        texts,
        [
            "[msg:10] image 1:",
            "<9/10/image/0>",
            "[msg:10] image 2: the picture can no longer be fetched",
            "[msg:10] sticker 1:",
            "<9/10/sticker/0>",
            "[msg:10] image 3: there is no such picture in that message",
        ]
    );
    // The provider reads the same bytes back for every later request, without fetching again.
    let calls = fetch.calls.load(Ordering::SeqCst);
    let loaded = store.load("9/10/image/0").await.unwrap();
    assert_eq!(
        (loaded.mime.as_str(), loaded.bytes.ends_with(b"a")),
        ("image/png", true)
    );
    assert_eq!(fetch.calls.load(Ordering::SeqCst), calls);
    assert!(
        store.load("9/10/voice/0").await.is_err(),
        "only pictures and stickers have keys"
    );

    // Messages outside the chat, and position 0, are argument errors.
    assert!(matches!(
        tool.call(
            &cx,
            OpenImagesArgs {
                pictures: vec![pic(11, 1, false)]
            }
        )
        .await,
        Err(ToolError::InvalidArguments(_))
    ));
    assert!(matches!(
        tool.call(
            &cx,
            OpenImagesArgs {
                pictures: vec![pic(10, 0, false)]
            }
        )
        .await,
        Err(ToolError::InvalidArguments(_))
    ));
    assert!(matches!(
        tool.call(&cx, OpenImagesArgs { pictures: vec![] }).await,
        Err(ToolError::InvalidArguments(_))
    ));
}
