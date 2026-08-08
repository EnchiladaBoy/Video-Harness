import type { AppSnapshot, UiEvent, UiEventEnvelope } from './types';

function draftsMatch(left: AppSnapshot['draft'], right: AppSnapshot['draft']): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function cloneDraft(draft: AppSnapshot['draft']): AppSnapshot['draft'] {
  return {
    ...draft,
    media: draft.media.map((item) => ({ ...item })),
    settings: { ...draft.settings }
  };
}

/**
 * Service snapshots carry authoritative catalogs, providers, jobs, and safety
 * state. A locally edited draft is the exception: catalog refreshes must not
 * paint an older backend draft over text that is still waiting for autosave.
 */
export function reconcileSnapshot(
  current: AppSnapshot,
  incoming: AppSnapshot,
  options: { preserveSelection?: boolean } = {}
): AppSnapshot {
  const next = structuredClone(incoming);
  const preserveSelection = options.preserveSelection ?? true;
  const selectedJobId = preserveSelection
    ? current.selectedJobId && next.jobs.some((job) => job.id === current.selectedJobId)
      ? current.selectedJobId
      : next.selectedJobId && next.jobs.some((job) => job.id === next.selectedJobId)
        ? next.selectedJobId
        : next.jobs[0]?.id
    : next.selectedJobId && next.jobs.some((job) => job.id === next.selectedJobId)
      ? next.selectedJobId
      : next.jobs[0]?.id;
  next.selectedJobId = selectedJobId;
  if (current.draftSaved || draftsMatch(current.draft, next.draft)) return next;

  return {
    ...next,
    draft: cloneDraft(current.draft),
    draftSaved: false,
    preparedReview: undefined
  };
}

export function applyUiEvent(snapshot: AppSnapshot, event: UiEvent): AppSnapshot {
  switch (event.type) {
    case 'snapshot_changed':
      return reconcileSnapshot(snapshot, event.snapshot);
    case 'provider_changed':
      return {
        ...snapshot,
        providers: snapshot.providers.map((provider) =>
          provider.id === event.provider.id ? event.provider : provider
        ),
        models: event.provider.connected
          ? snapshot.models
          : snapshot.models.filter((model) => model.providerId !== event.provider.id)
      };
    case 'review_ready':
      return { ...snapshot, preparedReview: event.review };
    case 'review_invalidated':
      return { ...snapshot, preparedReview: undefined };
    case 'job_added':
      return {
        ...snapshot,
        jobs: [event.job, ...snapshot.jobs.filter((job) => job.id !== event.job.id)],
        selectedJobId: event.job.id,
        preparedReview: undefined
      };
    case 'job_updated':
      return {
        ...snapshot,
        jobs: snapshot.jobs.map((job) => (job.id === event.job.id ? event.job : job))
      };
    case 'job_removed': {
      const jobs = snapshot.jobs.filter((job) => job.id !== event.jobId);
      return {
        ...snapshot,
        jobs,
        selectedJobId:
          snapshot.selectedJobId === event.jobId ? jobs[0]?.id : snapshot.selectedJobId
      };
    }
    case 'draft_saved':
      return { ...snapshot, draftSaved: event.revision === snapshot.draft.revision };
    case 'close_requested':
      return snapshot;
    case 'operation_failed':
      return event.operation === 'submission'
        ? { ...snapshot, preparedReview: undefined }
        : snapshot;
    case 'notice':
      return snapshot;
  }
}

export function applySequencedEvent(
  snapshot: AppSnapshot,
  currentSeq: number,
  envelope: UiEventEnvelope
): { snapshot: AppSnapshot; seq: number; gap: boolean } {
  if (envelope.seq <= currentSeq) {
    return { snapshot, seq: currentSeq, gap: false };
  }

  if (envelope.seq !== currentSeq + 1) {
    return { snapshot, seq: currentSeq, gap: true };
  }

  return {
    snapshot: applyUiEvent(snapshot, envelope.event),
    seq: envelope.seq,
    gap: false
  };
}

/** Lifecycle requests are edge-triggered and are not represented in a
 * snapshot, so callers must handle them even when sequence recovery drops the
 * corresponding state event. */
export function requiresImmediateHandling(
  event: UiEvent
): event is Extract<UiEvent, { type: 'close_requested' }> {
  return event.type === 'close_requested';
}
