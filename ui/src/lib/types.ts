export type ProviderId = 'openrouter' | 'fal';
export type MediaKind = 'image' | 'video' | 'audio';
export type MediaRole =
  | 'reference'
  | 'start_frame'
  | 'end_frame'
  | 'video_reference'
  | 'audio_reference';
export type WorkspaceView = 'create' | 'jobs' | 'providers';
export type JobStatus =
  | 'preparing'
  | 'queued'
  | 'processing'
  | 'downloading'
  | 'completed'
  | 'paused'
  | 'attention';

export interface ProviderSummary {
  id: ProviderId;
  name: string;
  shortName: string;
  connected: boolean;
  credentialStorage: 'keyring' | 'memory' | 'none';
  accountLabel?: string;
  description: string;
  localMediaNote: string;
}

export interface ModelCapabilities {
  images: boolean;
  video: boolean;
  audioReferences: boolean;
  generatedAudio: boolean;
}

export interface ModelSummary {
  id: string;
  providerId: ProviderId;
  name: string;
  description: string;
  capabilities: ModelCapabilities;
  durationOptions: string[];
  resolutionOptions: string[];
  aspectRatioOptions: string[];
  priceHint: string;
}

export interface MediaItem {
  handle: string;
  displayName: string;
  kind: MediaKind;
  role: MediaRole;
  source: 'local' | 'remote';
  detail: string;
  previewUrl?: string;
}

export interface GenerationSettings {
  duration: string;
  resolution: string;
  aspectRatio: string;
  generatedAudio: 'provider_default' | 'on' | 'off';
  seed: string;
}

export interface GenerationDraft {
  revision: number;
  providerId: ProviderId;
  modelId: string;
  prompt: string;
  media: MediaItem[];
  settings: GenerationSettings;
}

export interface PreparedReview {
  preparedId: number;
  revision: number;
  providerId: ProviderId;
  providerName: string;
  modelId: string;
  modelName: string;
  prompt: string;
  settings: GenerationSettings;
  media: MediaItem[];
  estimatedCost: string;
  expiresAt: string;
  uploadDisclosure?: string;
  advancedSettingsJson?: string;
}

export interface SafetyHoldSummary {
  handle: string;
  providerId: ProviderId;
  providerName: string;
  recordedAt: string;
  message: string;
}

export interface JobSummary {
  id: string;
  providerId: ProviderId;
  providerName: string;
  modelName: string;
  prompt: string;
  status: JobStatus;
  statusLabel: string;
  detail: string;
  createdAt: string;
  elapsedSeconds?: number;
  nextPollSeconds?: number;
  progress?: number;
  outputFileName?: string;
  hasLocalOutput: boolean;
  playbackUrl?: string;
  captionsUrl?: string;
  remoteContinues?: boolean;
  providerJobId?: string;
  deletable: boolean;
}

export interface AppSnapshot {
  providers: ProviderSummary[];
  models: ModelSummary[];
  draft: GenerationDraft;
  jobs: JobSummary[];
  selectedJobId?: string;
  preparedReview?: PreparedReview;
  draftSaved: boolean;
  safetyHolds: SafetyHoldSummary[];
}

export type UiEvent =
  | { type: 'snapshot_changed'; snapshot: AppSnapshot }
  | { type: 'provider_changed'; provider: ProviderSummary }
  | { type: 'review_ready'; review: PreparedReview }
  | { type: 'review_invalidated'; revision: number }
  | { type: 'job_added'; job: JobSummary }
  | { type: 'job_updated'; job: JobSummary }
  | { type: 'job_removed'; jobId: string }
  | { type: 'draft_saved'; revision: number }
  | {
      type: 'operation_failed';
      operation: 'preparation' | 'submission';
      message: string;
    }
  | { type: 'notice'; tone: 'neutral' | 'good' | 'warning' | 'danger'; message: string };

export interface UiEventEnvelope {
  seq: number;
  event: UiEvent;
}

export interface OpenSessionResult {
  seq: number;
  snapshot: AppSnapshot;
}

export interface PlaybackGrant {
  grantId: string;
  url: string;
}

export interface FileDropEvent {
  type: 'over' | 'drop' | 'cancel';
  paths: string[];
  position?: { x: number; y: number };
}

export interface BridgeSubscription {
  close(): void;
}

export interface VideoHarnessBridge {
  readonly mode: 'tauri' | 'browser-demo';
  openSession(onEvent: (envelope: UiEventEnvelope) => void): Promise<OpenSessionResult>;
  getSnapshot(): Promise<OpenSessionResult>;
  watchFileDrops(onDrop: (event: FileDropEvent) => void): Promise<BridgeSubscription>;
  connectProvider(providerId: ProviderId, key: string, persistOnSuccess: boolean): Promise<void>;
  forgetProvider(providerId: ProviderId): Promise<void>;
  acknowledgeSafetyHold(handle: string): Promise<void>;
  chooseMedia(): Promise<MediaItem[]>;
  attachDroppedMedia(paths: string[]): Promise<MediaItem[]>;
  addRemoteMedia(url: string, kind: MediaKind, role: MediaRole): Promise<MediaItem>;
  prepareGeneration(
    draft: GenerationDraft,
    authorization: { localMediaUploadConfirmed: boolean }
  ): Promise<void>;
  submitPrepared(preparedId: number): Promise<void>;
  invalidatePrepared(revision: number): Promise<void>;
  saveDraft(draft: GenerationDraft): Promise<void>;
  pauseJob(jobId: string): Promise<void>;
  resumeJob(jobId: string): Promise<void>;
  deleteRender(jobId: string, deleteOutput: boolean): Promise<void>;
  openOutput(jobId: string): Promise<void>;
  grantPlayback(jobId: string): Promise<PlaybackGrant>;
  releasePlayback(grantId: string): Promise<void>;
}

export function isActiveJob(status: JobStatus): boolean {
  return status === 'preparing' || status === 'queued' || status === 'processing' || status === 'downloading';
}

export function providerById(snapshot: AppSnapshot, id: ProviderId): ProviderSummary | undefined {
  return snapshot.providers.find((provider) => provider.id === id);
}

export function modelById(snapshot: AppSnapshot, id: string): ModelSummary | undefined {
  return snapshot.models.find((model) => model.id === id);
}
