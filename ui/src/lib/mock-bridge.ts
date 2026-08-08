import { applyUiEvent } from './state';
import { modelById } from './types';
import type {
  AppSnapshot,
  BridgeSubscription,
  FileDropEvent,
  GenerationDraft,
  JobSummary,
  MediaItem,
  MediaKind,
  MediaRole,
  PlaybackGrant,
  PreparedReview,
  ProviderId,
  UiEvent,
  UiEventEnvelope,
  VideoHarnessBridge
} from './types';

const flowerPoster = '/demo-poster.svg';

export function demoSnapshot(): AppSnapshot {
  return {
    providers: [
      {
        id: 'openrouter',
        name: 'OpenRouter',
        shortName: 'OR',
        connected: true,
        credentialStorage: 'keyring',
        accountLabel: 'Demo workspace',
        description: 'A front door to video models from many labs.',
        localMediaNote:
          'OpenRouter takes public links. Local files can travel through fal.ai after you approve the upload.'
      },
      {
        id: 'fal',
        name: 'fal.ai',
        shortName: 'fal',
        connected: false,
        credentialStorage: 'none',
        description: 'Video models, plus a bridge for local references.',
        localMediaNote:
          'Local files upload only in Review, as public-by-link files with a requested 24-hour expiry.'
      }
    ],
    models: [
      {
        id: 'black-forest-labs/flux-3-video',
        providerId: 'openrouter',
        name: 'FLUX 3 Video — Cinematic Image-to-Video, Audio & Extended Motion Preview',
        description:
          'A cinematic model for expressive camera motion, strong prompt adherence, and image-guided scenes.',
        capabilities: { images: true, video: false, audioReferences: false, generatedAudio: true, seed: true },
        durationOptions: ['5 seconds', '8 seconds', '10 seconds'],
        resolutionOptions: ['720p', '1080p'],
        aspectRatioOptions: ['16:9', '9:16', '1:1'],
        sizeOptions: ['1280x720'],
        supportedImageRoles: ['reference', 'start_frame', 'end_frame'],
        mediaConstraints: [{ kind: 'image', required: false, maxItems: 4 }],
        maxMediaItems: 4,
        audioRequiresVisual: false,
        framesExclusiveWithReferences: true,
        priceHint: 'From about $0.32'
      },
      {
        id: 'google/veo-3.1-fast',
        providerId: 'openrouter',
        name: 'Veo 3.1 Fast',
        description: 'Fast iteration with generated sound and polished natural motion.',
        capabilities: { images: true, video: true, audioReferences: false, generatedAudio: true, seed: true },
        durationOptions: ['4 seconds', '6 seconds', '8 seconds'],
        resolutionOptions: ['720p', '1080p'],
        aspectRatioOptions: ['16:9', '9:16'],
        sizeOptions: [],
        supportedImageRoles: ['reference', 'start_frame', 'end_frame'],
        mediaConstraints: [
          { kind: 'image', required: false, maxItems: 4 },
          { kind: 'video', required: false, maxItems: 1 }
        ],
        maxMediaItems: 5,
        audioRequiresVisual: false,
        framesExclusiveWithReferences: true,
        priceHint: 'From about $0.40'
      },
      {
        id: 'fal-ai/kling-video/v2.1/master/image-to-video',
        providerId: 'fal',
        name: 'Kling 2.1 Master Image-to-Video',
        description: 'High-fidelity image animation with deliberate camera direction.',
        capabilities: { images: true, video: false, audioReferences: false, generatedAudio: false, seed: false },
        durationOptions: ['5 seconds', '10 seconds'],
        resolutionOptions: ['720p', '1080p'],
        aspectRatioOptions: ['Use source', '16:9', '9:16', '1:1'],
        sizeOptions: [],
        supportedImageRoles: ['start_frame'],
        mediaConstraints: [{ kind: 'image', required: true, minItems: 1, maxItems: 1 }],
        maxMediaItems: 1,
        audioRequiresVisual: false,
        framesExclusiveWithReferences: false,
        priceHint: 'From about $0.28'
      },
      {
        id: 'fal-ai/wan/v2.2-a14b/video-to-video',
        providerId: 'fal',
        name: 'Wan 2.2 A14B Video-to-Video — Creative Restyle (High Quality)',
        description: 'Restyle a source clip while retaining broad composition and motion.',
        capabilities: { images: true, video: true, audioReferences: true, generatedAudio: false, seed: true },
        durationOptions: ['Use source', '5 seconds'],
        resolutionOptions: ['480p', '720p'],
        aspectRatioOptions: ['Use source'],
        sizeOptions: [],
        supportedImageRoles: ['reference'],
        mediaConstraints: [
          { kind: 'image', required: false, maxItems: 4 },
          { kind: 'video', required: true, minItems: 1, maxItems: 1 },
          { kind: 'audio', required: false, maxItems: 1 }
        ],
        maxMediaItems: 6,
        audioRequiresVisual: true,
        framesExclusiveWithReferences: false,
        priceHint: 'Usage based'
      }
    ],
    draft: {
      revision: 12,
      providerId: 'openrouter',
      modelId: 'black-forest-labs/flux-3-video',
      prompt:
        'A tiny midnight cinema floats above a quiet sea of clouds. Warm projector light spills through the windows as the camera drifts closer.',
      media: [
        {
          handle: 'demo-cloud-frame',
          displayName: 'cloud-cinema-start.png',
          kind: 'image',
          role: 'start_frame',
          source: 'local',
          detail: 'PNG · 1920 × 1080',
          previewUrl: flowerPoster
        }
      ],
      settings: {
        duration: '8 seconds',
        resolution: '1080p',
        aspectRatio: '16:9',
        size: '',
        generatedAudio: 'on',
        seed: '',
        advancedJson: ''
      }
    },
    jobs: [
      {
        id: 'job-or-8ca9f214',
        providerId: 'openrouter',
        providerName: 'OpenRouter',
        modelName: 'FLUX 3 Video',
        prompt: 'A paper airship sailing through peach-colored clouds at sunrise.',
        status: 'processing',
        statusLabel: 'Generating frames',
        detail: 'Tiny Cloud Cinema is keeping watch while the model works.',
        createdAt: new Date(Date.now() - 184_000).toISOString(),
        elapsedSeconds: 184,
        nextPollSeconds: 18,
        hasLocalOutput: false,
        deletable: false,
        monitorState: 'active',
        canResume: false,
        canPause: true
      },
      {
        id: 'job-fal-38c2a011',
        providerId: 'fal',
        providerName: 'fal.ai',
        modelName: 'Kling 2.1 Master',
        prompt: 'Macro wildflowers swaying in a soft summer storm.',
        status: 'completed',
        statusLabel: 'Ready to watch',
        detail: 'Your finished video is waiting in the Videos folder.',
        createdAt: new Date(Date.now() - 2_740_000).toISOString(),
        elapsedSeconds: 231,
        outputFileName: 'wildflowers-summer-storm.mp4',
        hasLocalOutput: true,
        providerJobId: 'fal-request-38c2a011-long-but-fully-visible',
        deletable: true,
        monitorState: 'terminal',
        canResume: false,
        canPause: false
      },
      {
        id: 'job-or-paused-1290',
        providerId: 'openrouter',
        providerName: 'OpenRouter',
        modelName: 'Veo 3.1 Fast',
        prompt: 'A glass greenhouse glowing in a snowy field at blue hour.',
        status: 'paused',
        statusLabel: 'Monitoring paused',
        detail: 'The provider keeps working while local updates take a nap.',
        createdAt: new Date(Date.now() - 5_420_000).toISOString(),
        elapsedSeconds: 92,
        remoteContinues: true,
        hasLocalOutput: false,
        deletable: false,
        monitorState: 'paused',
        canResume: true,
        canPause: false
      }
    ],
    selectedJobId: 'job-or-8ca9f214',
    draftSaved: true,
    safetyHolds: []
  };
}

function wait(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}

function inferKind(path: string): MediaKind {
  const lowered = path.toLowerCase();
  if (/\.(mp4|mov|webm)$/.test(lowered)) return 'video';
  if (/\.(mp3|wav|m4a)$/.test(lowered)) return 'audio';
  return 'image';
}

function itemFromPath(path: string, index: number): MediaItem {
  const displayName = path.split(/[\\/]/).pop() || `reference-${index + 1}`;
  const kind = inferKind(displayName);
  return {
    handle: `demo-file-${Date.now()}-${index}`,
    displayName,
    kind,
    role: kind === 'video' ? 'video_reference' : kind === 'audio' ? 'audio_reference' : 'reference',
    source: 'local',
    detail: `${kind === 'image' ? 'Image' : kind === 'video' ? 'Video' : 'Audio'} · demo selection`
  };
}

class BrowserDemoBridge implements VideoHarnessBridge {
  readonly mode = 'browser-demo' as const;
  private snapshot = demoSnapshot();
  private sequence = 7;
  private listeners = new Set<(event: UiEventEnvelope) => void>();
  private mediaPick = 0;

  private emit(event: UiEvent): void {
    this.snapshot = applyUiEvent(this.snapshot, event);
    const envelope = { seq: ++this.sequence, event };
    for (const listener of this.listeners) listener(envelope);
  }

  async openSession(onEvent: (envelope: UiEventEnvelope) => void) {
    this.listeners.add(onEvent);
    return {
      seq: this.sequence,
      snapshot: structuredClone(this.snapshot),
      preparing: false,
      submitting: false
    };
  }

  async getSnapshot() {
    return {
      seq: this.sequence,
      snapshot: structuredClone(this.snapshot),
      preparing: false,
      submitting: false
    };
  }

  async watchFileDrops(_onDrop: (event: FileDropEvent) => void): Promise<BridgeSubscription> {
    return { close: () => undefined };
  }

  async connectProvider(providerId: ProviderId, _key: string, persistOnSuccess: boolean) {
    await wait(550);
    const current = this.snapshot.providers.find((provider) => provider.id === providerId);
    if (!current) throw new Error('That provider is not available in this demo.');
    this.emit({
      type: 'provider_changed',
      provider: {
        ...current,
        connected: true,
        credentialStorage: persistOnSuccess ? 'keyring' : 'memory',
        accountLabel: 'Demo connection'
      }
    });
    this.emit({ type: 'notice', tone: 'good', message: `${current.name} connected in demo mode.` });
  }

  async forgetProvider(providerId: ProviderId) {
    await wait(260);
    const current = this.snapshot.providers.find((provider) => provider.id === providerId);
    if (!current) return;
    this.emit({
      type: 'provider_changed',
      provider: { ...current, connected: false, credentialStorage: 'none', accountLabel: undefined }
    });
    this.emit({ type: 'notice', tone: 'neutral', message: `${current.name} demo credential removed.` });
  }

  async acknowledgeSafetyHold(handle: string): Promise<void> {
    await wait(180);
    this.snapshot = {
      ...this.snapshot,
      safetyHolds: this.snapshot.safetyHolds.filter((hold) => hold.handle !== handle)
    };
    this.emit({ type: 'snapshot_changed', snapshot: structuredClone(this.snapshot) });
    this.emit({ type: 'notice', tone: 'good', message: 'Demo safety hold acknowledged.' });
  }

  async chooseMedia(): Promise<MediaItem[]> {
    await wait(180);
    const samples = [
      ['/home/demo/Videos/lantern-reference.mov'],
      ['/home/demo/Pictures/mountain-light.png', '/home/demo/Music/wind-texture.wav']
    ];
    const paths = samples[this.mediaPick++ % samples.length];
    return paths.map(itemFromPath);
  }

  async attachDroppedMedia(paths: string[]): Promise<MediaItem[]> {
    await wait(120);
    return paths.map(itemFromPath);
  }

  async addRemoteMedia(url: string, kind: MediaKind, role: MediaRole): Promise<MediaItem> {
    await wait(120);
    const parsed = new URL(url);
    return {
      handle: `demo-url-${Date.now()}`,
      displayName: parsed.pathname.split('/').pop() || parsed.hostname,
      kind,
      role,
      source: 'remote',
      detail: parsed.hostname,
      displayUrl: `${parsed.origin}${parsed.pathname}`
    };
  }

  async prepareGeneration(
    draft: GenerationDraft,
    authorization: { localMediaUploadConfirmed: boolean }
  ): Promise<void> {
    await wait(720);
    const provider = this.snapshot.providers.find((item) => item.id === draft.providerId);
    const model = modelById(this.snapshot, draft.providerId, draft.modelId);
    if (!provider?.connected) throw new Error(`Connect ${provider?.name ?? 'the provider'} before Review.`);
    if (!model) throw new Error('Choose an available model before Review.');
    if (
      draft.providerId === 'openrouter' &&
      draft.media.some((item) => item.source === 'local') &&
      !authorization.localMediaUploadConfirmed
    ) {
      throw new Error('Local-media staging was not confirmed. No files were uploaded.');
    }

    const review: PreparedReview = {
      preparedId: Date.now(),
      revision: draft.revision,
      providerId: provider.id,
      providerName: provider.name,
      modelId: model.id,
      modelName: model.name,
      prompt: draft.prompt,
      settings: structuredClone(draft.settings),
      media: structuredClone(draft.media),
      estimatedCost: model.priceHint.replace('From about ', '') || 'Provider estimate unavailable',
      expiresAt: new Date(Date.now() + 5 * 60_000).toISOString(),
      uploadDisclosure:
        draft.providerId === 'openrouter' && draft.media.some((item) => item.source === 'local')
          ? 'Your local references will be uploaded to fal.ai as public-by-link files with a requested 24-hour expiry, then their URLs will be shared with OpenRouter and the selected model provider.'
          : undefined
    };
    this.emit({ type: 'review_ready', review });
  }

  async submitPrepared(preparedId: number): Promise<void> {
    const review = this.snapshot.preparedReview;
    if (!review || review.preparedId !== preparedId) throw new Error('This Review is no longer current.');
    await wait(420);
    const id = `demo-${Date.now().toString(36)}`;
    const job: JobSummary = {
      id,
      providerId: review.providerId,
      providerName: review.providerName,
      modelName: review.modelName,
      prompt: review.prompt,
      status: 'queued',
      statusLabel: 'Waiting in line',
      detail: 'This render is pretend—nothing was sent and no credits were used.',
      createdAt: new Date().toISOString(),
      elapsedSeconds: 0,
      nextPollSeconds: 4,
      hasLocalOutput: false,
      deletable: false,
      monitorState: 'active',
      canResume: false,
      canPause: true
    };
    this.emit({ type: 'job_added', job });
    this.emit({ type: 'notice', tone: 'good', message: 'The pretend projector is rolling—no credits used.' });

    window.setTimeout(() => {
      this.emit({
        type: 'job_updated',
        job: {
          ...job,
          status: 'processing',
          statusLabel: 'Painting the in-between moments',
          detail: 'Tiny Cloud Cinema is painting a pretend progress state.',
          elapsedSeconds: 2,
          nextPollSeconds: 3
        }
      });
    }, 1_500);
    window.setTimeout(() => {
      this.emit({
        type: 'job_updated',
        job: {
          ...job,
          status: 'completed',
          statusLabel: 'Pretend render complete',
          detail: 'In the desktop app, your finished file would be ready to play here.',
          elapsedSeconds: 6,
          outputFileName: 'demo-cloud-cinema.mp4',
          hasLocalOutput: true,
          deletable: true,
          monitorState: 'terminal',
          canResume: false,
          canPause: false
        }
      });
    }, 6_000);
  }

  async invalidatePrepared(revision: number): Promise<void> {
    if (this.snapshot.preparedReview) this.emit({ type: 'review_invalidated', revision });
  }

  async saveDraft(draft: GenerationDraft): Promise<void> {
    this.snapshot = { ...this.snapshot, draft: structuredClone(draft), draftSaved: true };
    this.emit({ type: 'draft_saved', revision: draft.revision });
  }

  async acknowledgeCloseRequest(_requestId: number): Promise<void> {
    return undefined;
  }

  async cancelCloseRequest(_requestId: number): Promise<void> {
    return undefined;
  }

  async saveDraftAndClose(draft: GenerationDraft, _requestId: number): Promise<void> {
    await this.saveDraft(draft);
  }

  async pauseJob(jobId: string): Promise<void> {
    const job = this.snapshot.jobs.find((item) => item.id === jobId);
    if (!job) return;
    this.emit({
      type: 'job_updated',
      job: {
        ...job,
        status: 'paused',
        statusLabel: 'Monitoring paused',
        detail: 'The pretend provider keeps working while local updates take a nap.',
        remoteContinues: true,
        monitorState: 'paused',
        canResume: true,
        canPause: false
      }
    });
  }

  async resumeJob(jobId: string): Promise<void> {
    const job = this.snapshot.jobs.find((item) => item.id === jobId);
    if (!job) return;
    this.emit({
      type: 'job_updated',
      job: {
        ...job,
        status: 'processing',
        statusLabel: 'Back on watch',
        detail: 'Tiny Cloud Cinema is checking again.',
        remoteContinues: undefined,
        nextPollSeconds: 8,
        monitorState: 'active',
        canResume: false,
        canPause: true
      }
    });
  }

  async deleteRender(jobId: string, _deleteOutput: boolean): Promise<void> {
    await wait(220);
    const job = this.snapshot.jobs.find((item) => item.id === jobId);
    if (!job?.deletable) throw new Error('Only finished renders can be removed from the reel.');
    this.emit({ type: 'job_removed', jobId });
  }

  async openOutput(_jobId: string): Promise<void> {
    throw new Error('Demo mode has no real file to open.');
  }

  async grantPlayback(jobId: string): Promise<PlaybackGrant> {
    return { grantId: `demo-playback-${jobId}`, url: '' };
  }

  async releasePlayback(_grantId: string): Promise<void> {
    return undefined;
  }
}

export function createMockBridge(): VideoHarnessBridge {
  return new BrowserDemoBridge();
}
