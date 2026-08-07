import { Channel, invoke } from '@tauri-apps/api/core';
import { createMockBridge } from './mock-bridge';
import type {
  AppSnapshot,
  BridgeSubscription,
  FileDropEvent,
  GenerationDraft,
  MediaItem,
  MediaKind,
  MediaRole,
  OpenSessionResult,
  PlaybackGrant,
  ProviderId,
  UiEventEnvelope,
  VideoHarnessBridge
} from './types';

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && window.__TAURI_INTERNALS__ !== undefined;
}

class TauriBridge implements VideoHarnessBridge {
  readonly mode = 'tauri' as const;

  async openSession(onEvent: (envelope: UiEventEnvelope) => void): Promise<OpenSessionResult> {
    const eventChannel = new Channel<UiEventEnvelope>();
    eventChannel.onmessage = onEvent;
    return invoke<OpenSessionResult>('open_session', { onEvent: eventChannel });
  }

  getSnapshot(): Promise<OpenSessionResult> {
    return invoke('get_snapshot');
  }

  async watchFileDrops(onDrop: (event: FileDropEvent) => void): Promise<BridgeSubscription> {
    const { getCurrentWebview } = await import('@tauri-apps/api/webview');
    const unlisten = await getCurrentWebview().onDragDropEvent((event) => {
      const payload = event.payload;
      if (payload.type === 'drop') {
        onDrop({ type: 'drop', paths: payload.paths, position: payload.position });
      } else if (payload.type === 'over') {
        onDrop({ type: 'over', paths: [], position: payload.position });
      } else {
        onDrop({ type: 'cancel', paths: [] });
      }
    });
    return { close: unlisten };
  }

  connectProvider(providerId: ProviderId, key: string, persistOnSuccess: boolean): Promise<void> {
    return invoke('connect_provider', { providerId, key, persistOnSuccess });
  }

  forgetProvider(providerId: ProviderId): Promise<void> {
    return invoke('forget_provider', { providerId });
  }

  acknowledgeSafetyHold(handle: string): Promise<void> {
    return invoke('acknowledge_safety_hold', { handle });
  }

  chooseMedia(): Promise<MediaItem[]> {
    return invoke('choose_media');
  }

  attachDroppedMedia(paths: string[]): Promise<MediaItem[]> {
    return invoke('attach_dropped_media', { paths });
  }

  addRemoteMedia(url: string, kind: MediaKind, role: MediaRole): Promise<MediaItem> {
    return invoke('add_remote_media', { url, kind, role });
  }

  prepareGeneration(
    draft: GenerationDraft,
    authorization: { localMediaUploadConfirmed: boolean }
  ): Promise<void> {
    return invoke('prepare_generation', { draft, ...authorization });
  }

  submitPrepared(preparedId: number): Promise<void> {
    return invoke('submit_prepared', { preparedId });
  }

  invalidatePrepared(revision: number): Promise<void> {
    return invoke('invalidate_prepared', { revision });
  }

  saveDraft(draft: GenerationDraft): Promise<void> {
    return invoke('save_draft', { draft });
  }

  pauseJob(jobId: string): Promise<void> {
    return invoke('pause_job', { jobId });
  }

  resumeJob(jobId: string): Promise<void> {
    return invoke('resume_job', { jobId });
  }

  deleteRender(jobId: string, deleteOutput: boolean): Promise<void> {
    return invoke('delete_render', { jobId, deleteOutput });
  }

  openOutput(jobId: string): Promise<void> {
    return invoke('open_output', { jobId });
  }

  grantPlayback(jobId: string): Promise<PlaybackGrant> {
    return invoke('grant_playback', { jobId });
  }

  releasePlayback(grantId: string): Promise<void> {
    return invoke('release_playback', { grantId });
  }
}

export function createBridge(): VideoHarnessBridge {
  return isTauriRuntime() ? new TauriBridge() : createMockBridge();
}

export function cloneSnapshot(snapshot: AppSnapshot): AppSnapshot {
  return structuredClone(snapshot);
}
