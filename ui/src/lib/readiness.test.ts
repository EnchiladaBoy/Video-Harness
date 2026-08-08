import { describe, expect, it } from 'vitest';
import { demoSnapshot } from './mock-bridge';
import {
  advancedJsonIssue,
  constrainMediaAppend,
  modelSupportsImages,
  normalizeDraftForModel,
  reviewReadinessIssue
} from './readiness';
import { modelById } from './types';

function reviewableSnapshot() {
  const snapshot = demoSnapshot();
  snapshot.providers = snapshot.providers.map((provider) => ({ ...provider, connected: true }));
  snapshot.draft.media = [];
  return snapshot;
}

describe('Review readiness', () => {
  it('rejects unsupported roles and duplicate frame roles before preparation', () => {
    const snapshot = reviewableSnapshot();
    snapshot.draft.media = [
      {
        handle: 'one',
        displayName: 'one.png',
        kind: 'image',
        role: 'start_frame',
        source: 'remote',
        detail: 'image'
      },
      {
        handle: 'two',
        displayName: 'two.png',
        kind: 'image',
        role: 'start_frame',
        source: 'remote',
        detail: 'image'
      }
    ];

    expect(reviewReadinessIssue(snapshot, true)).toBe('Use at most one start frame.');

    snapshot.draft.providerId = 'fal';
    snapshot.draft.modelId = 'fal-ai/kling-video/v2.1/master/image-to-video';
    snapshot.draft.media = [{ ...snapshot.draft.media[0], role: 'reference' }];
    expect(reviewReadinessIssue(snapshot, true)).toBe('Choose a supported role for one.png.');
  });

  it('requires every advertised image role without exposing provider binding names', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.requiredImageRoles = ['start_frame', 'end_frame'];
    model.mediaConstraints = [];
    model.maxMediaItems = undefined;
    model.audioRequiresVisual = false;
    model.framesExclusiveWithReferences = false;
    snapshot.draft.media = [
      {
        handle: 'start',
        displayName: 'start.png',
        kind: 'image',
        role: 'start_frame',
        source: 'remote',
        detail: 'image'
      }
    ];

    expect(reviewReadinessIssue(snapshot, true)).toBe('This model requires an end frame.');

    snapshot.draft.media.push({
      ...snapshot.draft.media[0],
      handle: 'end',
      displayName: 'end.png',
      role: 'end_frame'
    });
    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();
  });

  it('accepts dedicated frame inputs for frame-only models', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.capabilities.images = false;
    model.supportedImageRoles = ['start_frame'];
    model.requiredImageRoles = [];
    model.mediaConstraints = [];
    snapshot.draft.media = [
      {
        handle: 'frame',
        displayName: 'opening.png',
        kind: 'image',
        role: 'start_frame',
        source: 'remote',
        detail: 'image'
      }
    ];

    expect(modelSupportsImages(model)).toBe(true);
    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();
  });

  it('allocates frame-only roles and enforces model media budgets while appending', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.capabilities.images = false;
    model.supportedImageRoles = ['start_frame', 'end_frame'];
    model.requiredImageRoles = ['start_frame'];
    model.maxMediaItems = 2;
    model.mediaConstraints = [
      {
        kind: 'image',
        roles: ['start_frame', 'end_frame'],
        required: false,
        maxItems: 2
      }
    ];
    const genericImage = {
      handle: 'one',
      displayName: 'one.png',
      kind: 'image' as const,
      role: 'reference' as const,
      source: 'local' as const,
      detail: 'image'
    };

    const result = constrainMediaAppend(
      [],
      [
        genericImage,
        { ...genericImage, handle: 'two', displayName: 'two.png' },
        { ...genericImage, handle: 'three', displayName: 'three.png' }
      ],
      model
    );

    expect(result.accepted.map((item) => item.role)).toEqual(['start_frame', 'end_frame']);
    expect(result.skipped).toBe(1);

    model.maxMediaItems = 1;
    const singleFrame = constrainMediaAppend([], [genericImage, { ...genericImage, handle: 'two' }], model);
    expect(singleFrame.accepted).toHaveLength(1);
    expect(singleFrame.accepted[0]?.role).toBe('start_frame');
    expect(singleFrame.skipped).toBe(1);
  });

  it('applies per-kind and role media maxima before adding items to a draft', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.capabilities.audioReferences = true;
    model.mediaConstraints = [
      { kind: 'image', roles: ['reference'], required: false, maxItems: 1 },
      { kind: 'audio', required: false, maxItems: 0 }
    ];
    model.maxMediaItems = 4;
    const existing = [
      {
        handle: 'existing',
        displayName: 'existing.png',
        kind: 'image' as const,
        role: 'reference' as const,
        source: 'remote' as const,
        detail: 'image'
      }
    ];

    const result = constrainMediaAppend(
      existing,
      [
        { ...existing[0], handle: 'extra' },
        {
          ...existing[0],
          handle: 'frame',
          role: 'start_frame' as const
        },
        {
          ...existing[0],
          handle: 'audio',
          kind: 'audio' as const,
          role: 'audio_reference' as const
        }
      ],
      model
    );

    expect(result.accepted.map((item) => item.handle)).toEqual(['frame']);
    expect(result.skipped).toBe(2);
  });

  it('enforces model media cardinality and catalog options', () => {
    const snapshot = reviewableSnapshot();
    snapshot.draft.providerId = 'fal';
    snapshot.draft.modelId = 'fal-ai/wan/v2.2-a14b/video-to-video';
    snapshot.draft.settings.duration = 'Use source';
    snapshot.draft.settings.resolution = '480p';
    snapshot.draft.settings.aspectRatio = 'Use source';
    snapshot.draft.settings.generatedAudio = 'provider_default';

    expect(reviewReadinessIssue(snapshot, true)).toBe('This model requires a video reference.');

    snapshot.draft.media = [
      {
        handle: 'clip',
        displayName: 'clip.mp4',
        kind: 'video',
        role: 'video_reference',
        source: 'remote',
        detail: 'video'
      }
    ];
    snapshot.draft.settings.resolution = '4K';
    expect(reviewReadinessIssue(snapshot, true)).toMatch(/supported resolution/);
  });

  it('counts only roles that populate a provider media binding', () => {
    const snapshot = reviewableSnapshot();
    const model = snapshot.models.find(
      (item) =>
        item.providerId === snapshot.draft.providerId && item.id === snapshot.draft.modelId
    );
    if (!model) throw new Error('Missing selected model fixture');
    model.framesExclusiveWithReferences = false;
    model.mediaConstraints = [
      { kind: 'image', roles: ['reference'], required: true, minItems: 1, maxItems: 1 }
    ];
    snapshot.draft.media = [
      {
        handle: 'frame',
        displayName: 'opening.png',
        kind: 'image',
        role: 'start_frame',
        source: 'remote',
        detail: 'image'
      }
    ];

    expect(reviewReadinessIssue(snapshot, true)).toBe('This model requires an image reference.');

    snapshot.draft.media.push({
      ...snapshot.draft.media[0],
      handle: 'reference',
      displayName: 'style.png',
      role: 'reference'
    });
    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();
  });

  it('allows an optional media bucket to be absent but enforces its minimum when present', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.mediaConstraints = [
      {
        kind: 'image',
        roles: ['reference'],
        required: false,
        minItemsWhenPresent: 2,
        maxItems: 4
      }
    ];

    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();

    snapshot.draft.media = [
      {
        handle: 'one',
        displayName: 'one.png',
        kind: 'image',
        role: 'reference',
        source: 'remote',
        detail: 'image'
      }
    ];
    expect(reviewReadinessIssue(snapshot, true)).toBe(
      'This model requires at least 2 image references when that media type is used.'
    );

    snapshot.draft.media.push({
      ...snapshot.draft.media[0],
      handle: 'two',
      displayName: 'two.png'
    });
    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();
  });

  it('validates seed bounds and advanced JSON without blocking autosave text', () => {
    const snapshot = reviewableSnapshot();
    snapshot.draft.settings.seed = 'half-written';
    expect(reviewReadinessIssue(snapshot, true)).toBe('Seed must be a whole number.');

    snapshot.draft.settings.seed = '9223372036854775808';
    expect(reviewReadinessIssue(snapshot, true)).toMatch(/signed 64-bit/);

    snapshot.draft.settings.seed = '';
    snapshot.draft.settings.advancedJson = '[1, 2]';
    expect(reviewReadinessIssue(snapshot, true)).toBe(
      'Advanced settings must be a JSON object.'
    );
    expect(advancedJsonIssue('{"guidance": 4}')).toBeUndefined();
  });

  it('rejects nested credential keys without inspecting or echoing their values', () => {
    const sensitiveValue = 'must-never-appear-in-an-error';
    const issue = advancedJsonIssue(
      JSON.stringify({
        adapter: {
          layers: [{ authorization_header: sensitiveValue }]
        }
      })
    );

    expect(issue).toBe('Advanced settings may not contain credential fields.');
    expect(issue).not.toContain(sensitiveValue);
    expect(advancedJsonIssue('{"nested":{"client_secret":"redacted"}}')).toBe(
      'Advanced settings may not contain credential fields.'
    );
    expect(advancedJsonIssue('{"nested":{"password":"redacted"}}')).toBe(
      'Advanced settings may not contain credential fields.'
    );
    expect(advancedJsonIssue('{"database":{"passwd":"redacted"}}')).toBe(
      'Advanced settings may not contain credential fields.'
    );
  });

  it('allows token-count generation controls that are not credentials', () => {
    expect(
      advancedJsonIssue(
        JSON.stringify({ limits: { max_tokens: 128, token_count: 4 } })
      )
    ).toBeUndefined();
  });

  it('clears settings that a newly selected model cannot represent', () => {
    const snapshot = reviewableSnapshot();
    const kling = snapshot.models.find(
      (model) => model.providerId === 'fal' && model.id.includes('kling')
    );
    snapshot.draft.settings.generatedAudio = 'on';
    snapshot.draft.settings.seed = '42';
    snapshot.draft.settings.advancedJson = '{"model_specific": true}';

    normalizeDraftForModel(snapshot.draft, kling, {
      chooseDefaults: true,
      clearAdvanced: true
    });

    expect(snapshot.draft.settings).toMatchObject({
      duration: '5 seconds',
      resolution: '720p',
      aspectRatio: 'Use source',
      size: '',
      generatedAudio: 'provider_default',
      seed: '',
      advancedJson: ''
    });
  });

  it('resolves model capabilities within the selected provider', () => {
    const snapshot = reviewableSnapshot();
    const selected = snapshot.models.find(
      (model) =>
        model.providerId === snapshot.draft.providerId && model.id === snapshot.draft.modelId
    );
    if (!selected) throw new Error('Missing selected model fixture');
    const collision = {
      ...structuredClone(selected),
      providerId: 'fal' as const,
      capabilities: {
        images: false,
        video: false,
        audioReferences: false,
        generatedAudio: false,
        seed: false
      },
      durationOptions: ['not-the-selected-provider']
    };
    snapshot.models.unshift(collision);

    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();
    expect(modelById(snapshot, 'openrouter', selected.id)).toBe(selected);
    expect(modelById(snapshot, 'fal', selected.id)).toBe(collision);
  });

  it('enforces model-wide media policies before provider preparation', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.capabilities.audioReferences = true;
    model.audioRequiresVisual = true;
    model.maxMediaItems = 1;
    model.framesExclusiveWithReferences = true;
    snapshot.draft.media = [
      {
        handle: 'sound',
        displayName: 'ambience.wav',
        kind: 'audio',
        role: 'audio_reference',
        source: 'remote',
        detail: 'audio'
      }
    ];

    expect(reviewReadinessIssue(snapshot, true)).toBe(
      'Audio references for this model require at least one image or video reference.'
    );

    snapshot.draft.media.push({
      handle: 'visual',
      displayName: 'visual.png',
      kind: 'image',
      role: 'reference',
      source: 'remote',
      detail: 'image'
    });
    expect(reviewReadinessIssue(snapshot, true)).toBe(
      'This model accepts at most one reference item in total.'
    );

    model.maxMediaItems = 4;
    snapshot.draft.media = [
      { ...snapshot.draft.media[1], handle: 'frame', role: 'start_frame' },
      { ...snapshot.draft.media[0], handle: 'sound' }
    ];
    expect(reviewReadinessIssue(snapshot, true)).toBe(
      'Use either frame images or non-frame input references with this model, not both.'
    );
  });

  it('keeps exact size mutually exclusive with resolution and aspect ratio', () => {
    const snapshot = reviewableSnapshot();
    const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
    if (!model) throw new Error('Missing selected model fixture');
    model.sizeOptions = ['1280x720'];
    snapshot.draft.settings.size = '1280x720';

    expect(reviewReadinessIssue(snapshot, true)).toBe(
      'Choose either an exact output size or resolution and aspect ratio, not both.'
    );

    normalizeDraftForModel(snapshot.draft, model, { chooseDefaults: true });
    expect(snapshot.draft.settings).toMatchObject({
      size: '1280x720',
      resolution: '',
      aspectRatio: ''
    });
    expect(reviewReadinessIssue(snapshot, true)).toBeUndefined();

    snapshot.draft.settings.resolution = '1080p';
    normalizeDraftForModel(snapshot.draft, model);
    expect(snapshot.draft.settings.resolution).toBe('');
    expect(snapshot.draft.settings.size).toBe('1280x720');
  });
});
