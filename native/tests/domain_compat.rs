use std::fs;

use chrono::{Local, TimeZone, Utc};
use openrouter_video_studio::config::{make_output_path_at, partial_path, slugify_prompt};
use openrouter_video_studio::domain::{
    FrameImage, FrameType, InputReference, JobLocator, JobStatus, ModelRef, ProviderId,
    ProviderJobKey, VideoCatalog, VideoModel, VideoRequest, estimate_cost,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::tempdir;

fn fixture(name: &str) -> Value {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(manifest.join("fixtures").join(name)).expect("read fixture");
    serde_json::from_str(&text).expect("parse fixture JSON")
}

#[test]
fn request_fixtures_round_trip_without_inventing_optional_fields() {
    let requests = fixture("requests.json");
    let minimal = requests.get("minimal").expect("minimal request");
    let parsed = VideoRequest::from_payload(minimal).expect("parse minimal request");
    assert_eq!(parsed.to_payload().expect("serialize request"), *minimal);

    let configured = requests.get("configured").expect("configured request");
    let parsed = VideoRequest::from_payload(configured).expect("parse configured request");
    assert_eq!(parsed.to_payload().expect("serialize request"), *configured);
    assert_eq!(parsed.frame_images[0].frame_type, FrameType::FirstFrame);
    assert_eq!(parsed.input_references.len(), 1);
}

#[test]
fn request_payload_trims_text_and_validates_reference_urls_and_dimensions() {
    let mut request =
        VideoRequest::new(" example/video ", " drifting clouds ").expect("valid request");
    request.duration = Some(4);
    request.generate_audio = Some(false);
    request.frame_images.push(
        FrameImage::new("https://images.example/first.png", FrameType::FirstFrame)
            .expect("valid frame"),
    );
    request
        .input_references
        .push(InputReference::new("https://images.example/style.png").expect("valid reference"));
    let payload = request.to_payload().expect("serialize request");
    assert_eq!(payload["model"], "example/video");
    assert_eq!(payload["prompt"], "drifting clouds");
    assert_eq!(payload["generate_audio"], false);
    assert!(payload.get("resolution").is_none());

    for invalid in [
        "http://images.example/frame.png",
        "file:///tmp/frame.png",
        "https://user:password@images.example/frame.png",
        "not-a-url",
    ] {
        assert!(InputReference::new(invalid).is_err(), "accepted {invalid}");
    }

    request.size = Some("1280x720".into());
    request.resolution = Some("720p".into());
    assert!(request.validate().is_err());
}

#[test]
fn catalog_fixture_maps_capabilities_pricing_and_cache_compatibly() {
    let payload = fixture("catalog.json");
    let fetched_at = chrono::DateTime::parse_from_rfc3339(
        payload["fetched_at"].as_str().expect("fixture timestamp"),
    )
    .expect("parse timestamp")
    .with_timezone(&Utc);
    let catalog = VideoCatalog::from_api_at(&payload, fetched_at, false).expect("parse catalog");
    let flux = catalog
        .find("black-forest-labs/flux-3-video")
        .expect("Flux fixture model");
    assert_eq!(catalog.preferred(), Some(flux));
    assert_eq!(flux.supported_durations, vec![4, 8]);
    assert_eq!(flux.supported_resolutions, vec!["720p", "1080p"]);
    assert_eq!(flux.generate_audio, Some(false));
    assert_eq!(
        flux.pricing_skus["cents_per_second_output_720p"],
        Decimal::new(17, 0)
    );

    let directory = tempdir().expect("temporary directory");
    let cache = directory.path().join("nested/video-models.json");
    catalog.save(&cache).expect("save cache");
    let restored = VideoCatalog::load(&cache).expect("load cache");
    assert!(restored.stale);
    assert_eq!(restored.fetched_at, fetched_at);
    assert!(restored.find(&flux.id).is_some());
}

#[test]
fn model_reports_every_incompatible_setting() {
    let model = VideoModel::from_api(&json!({
        "id": "example/video-one",
        "supported_resolutions": ["720p"],
        "supported_aspect_ratios": ["16:9"],
        "supported_sizes": ["1280x720"],
        "supported_durations": [4],
        "supported_frame_images": ["first_frame"],
        "generate_audio": false,
        "seed": false
    }))
    .expect("parse model");
    let mut request = VideoRequest::new(&model.id, "test").expect("request");
    request.duration = Some(5);
    request.resolution = Some("4k".into());
    request.aspect_ratio = Some("1:1".into());
    request.generate_audio = Some(true);
    request.seed = Some(7);
    request.frame_images.push(
        FrameImage::new("https://images.example/last.png", FrameType::LastFrame).expect("frame"),
    );

    let problems = model.supports_request(&request);
    assert_eq!(problems.len(), 6);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("resolution"))
    );
    assert!(problems.iter().any(|problem| problem.contains("audio")));
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("last_frame"))
    );
}

#[test]
fn cost_estimates_only_known_units_and_never_guess_token_pricing() {
    let catalog = VideoCatalog::from_api(&fixture("catalog.json")).expect("catalog");
    let flux = catalog
        .find("black-forest-labs/flux-3-video")
        .expect("Flux model");
    let mut request = VideoRequest::new(&flux.id, "test").expect("request");
    request.duration = Some(5);
    request.resolution = Some("720p".into());
    let estimate = estimate_cost(flux, &request);
    assert_eq!(estimate.amount, Some(Decimal::new(85, 2)));
    assert!(estimate.exact);
    assert_eq!(
        estimate.pricing_sku.as_deref(),
        Some("cents_per_second_output_720p")
    );
    assert_eq!(estimate.unit_price, Some(Decimal::new(17, 2)));

    let token_model = VideoModel::from_api(&json!({
        "id": "example/token-video",
        "pricing_skus": {"video_tokens": "0.0001"}
    }))
    .expect("token model");
    assert!(estimate_cost(&token_model, &request).amount.is_none());

    let text_model = VideoModel::from_api(&json!({
        "id": "example/text-video",
        "pricing_skus": {"text_to_video_duration_seconds_720p": "0.20"}
    }))
    .expect("text model");
    request.model = text_model.id.clone();
    assert_eq!(
        estimate_cost(&text_model, &request).amount,
        Some(Decimal::new(100, 2))
    );
    request
        .input_references
        .push(InputReference::new("https://images.example/style.png").expect("reference"));
    assert!(estimate_cost(&text_model, &request).amount.is_none());
}

#[test]
fn job_fixtures_preserve_unknown_status_cost_and_provider_error() {
    let jobs = fixture("jobs.json");
    let completed = openrouter_video_studio::domain::VideoJob::from_api(&jobs["completed"])
        .expect("completed job");
    assert_eq!(completed.status, JobStatus::Completed);
    assert!(completed.terminal());
    assert!(completed.successful());
    assert_eq!(completed.cost(), Some(Decimal::new(85, 2)));

    let failed =
        openrouter_video_studio::domain::VideoJob::from_api(&jobs["failed"]).expect("failed job");
    assert_eq!(failed.error.as_deref(), Some("Fixture provider failed"));

    let unknown = openrouter_video_studio::domain::VideoJob::from_api(&json!({
        "id": "job-provider",
        "status": "provider-specific",
        "polling_url": "/api/v1/videos/job-provider"
    }))
    .expect("unknown status job");
    assert_eq!(
        unknown.status,
        JobStatus::Unknown("provider-specific".into())
    );
    assert!(!unknown.terminal());
}

#[test]
fn output_paths_are_portable_and_partial_collisions_are_reserved() {
    let directory = tempdir().expect("temporary directory");
    let now = Local
        .with_ymd_and_hms(2026, 8, 6, 14, 30, 15)
        .single()
        .expect("local timestamp");
    let first = make_output_path_at(
        "Clouds / stars: a café!",
        "job/../../unsafe",
        directory.path(),
        now,
        ".mp4",
    );
    assert_eq!(
        first.file_name().and_then(|name| name.to_str()),
        Some("20260806-143015-clouds-stars-a-cafe-job-unsafe.mp4")
    );
    fs::write(partial_path(&first), b"incomplete").expect("create partial collision");
    let second = make_output_path_at(
        "Clouds / stars: a café!",
        "job/../../unsafe",
        directory.path(),
        now,
        ".mp4",
    );
    assert_eq!(
        second.file_name().and_then(|name| name.to_str()),
        Some("20260806-143015-clouds-stars-a-cafe-job-unsafe-2.mp4")
    );
    assert_eq!(slugify_prompt("   🌧️ 🎬   ", 48), "video");
    assert!(!slugify_prompt("one/two", 48).contains('/'));
}

#[test]
fn provider_identifiers_and_locators_validate_on_deserialization() {
    assert!(serde_json::from_value::<ProviderId>(json!("../fal")).is_err());
    assert!(serde_json::from_value::<ProviderId>(json!("fal\nsecret")).is_err());
    assert!(
        serde_json::from_value::<ModelRef>(json!({
            "provider_id": "fal",
            "model_id": "\n"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ProviderJobKey>(json!({
            "provider_id": "fal",
            "remote_job_id": "\u{0}"
        }))
        .is_err()
    );

    let locator: JobLocator = serde_json::from_value(json!({
        "provider": "fal",
        "endpoint_id": "fal-ai/fixture/video",
        "request_id": "request-1",
        "status_url": "https://queue.fal.run/fal-ai/fixture/video/requests/request-1/status",
        "response_url": "https://queue.fal.run/fal-ai/fixture/video/requests/request-1/response"
    }))
    .expect("valid fal locator");
    assert_eq!(locator.provider_id(), ProviderId::fal());
    assert_eq!(locator.remote_job_id(), "request-1");
    assert!(
        serde_json::from_value::<JobLocator>(json!({
            "provider": "fal",
            "endpoint_id": "fal-ai/fixture/video",
            "request_id": "request-1",
            "response_url": "https://queue.fal.run/another/endpoint/requests/request-1/response"
        }))
        .is_err()
    );
}

#[test]
fn provider_scoped_keys_do_not_collide() {
    let openrouter = ProviderJobKey::new(ProviderId::openrouter(), "same-id").expect("key");
    let fal = ProviderJobKey::new(ProviderId::fal(), "same-id").expect("key");
    assert_ne!(openrouter, fal);
    let openrouter_model =
        ModelRef::new(ProviderId::openrouter(), "same/model").expect("model ref");
    let fal_model = ModelRef::new(ProviderId::fal(), "same/model").expect("model ref");
    assert_ne!(openrouter_model, fal_model);
}
