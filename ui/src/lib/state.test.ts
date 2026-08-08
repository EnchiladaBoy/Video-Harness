import { describe, expect, it } from 'vitest';
import { demoSnapshot } from './mock-bridge';
import {
  applySequencedEvent,
  applyUiEvent,
  reconcileSnapshot,
  requiresImmediateHandling
} from './state';
import { isActivelyMonitored } from './types';

describe('frontend event projection', () => {
  it('updates a provider without mutating the previous snapshot', () => {
    const before = demoSnapshot();
    const provider = { ...before.providers[1], connected: true as const };
    const after = applyUiEvent(before, { type: 'provider_changed', provider });

    expect(after.providers.find((item) => item.id === 'fal')?.connected).toBe(true);
    expect(before.providers.find((item) => item.id === 'fal')?.connected).toBe(false);
  });

  it('drops duplicate events and identifies a sequence gap', () => {
    const snapshot = demoSnapshot();
    const duplicate = applySequencedEvent(snapshot, 4, {
      seq: 4,
      event: { type: 'draft_saved', revision: snapshot.draft.revision }
    });
    const gap = applySequencedEvent(snapshot, 4, {
      seq: 7,
      event: { type: 'draft_saved', revision: snapshot.draft.revision }
    });

    expect(duplicate.snapshot).toBe(snapshot);
    expect(gap.gap).toBe(true);
    expect(gap.snapshot).toBe(snapshot);
    expect(gap.seq).toBe(4);
  });

  it('preserves close requests as lifecycle effects across sequence resync', () => {
    const snapshot = demoSnapshot();
    const event = { type: 'close_requested', requestId: 41 } as const;
    const gap = applySequencedEvent(snapshot, 4, { seq: 7, event });

    expect(gap.gap).toBe(true);
    expect(requiresImmediateHandling(event)).toBe(true);
    expect(event.requestId).toBe(41);
  });

  it('uses monitor state as the source of truth with a legacy status fallback', () => {
    const job = demoSnapshot().jobs[0];

    expect(isActivelyMonitored({ ...job, status: 'processing', monitorState: 'recoverable' })).toBe(
      false
    );
    expect(isActivelyMonitored({ ...job, status: 'paused', monitorState: 'active' })).toBe(true);
    expect(isActivelyMonitored({ ...job, status: 'queued', monitorState: undefined })).toBe(true);
  });

  it('keeps snapshot data unchanged for an operation failure', () => {
    const snapshot = demoSnapshot();
    const after = applyUiEvent(snapshot, {
      type: 'operation_failed',
      operation: 'preparation',
      message: 'Fixture failure'
    });

    expect(after).toBe(snapshot);
  });

  it('removes a consumed Review after submission failure', () => {
    const snapshot = demoSnapshot();
    snapshot.preparedReview = {
      preparedId: 9,
      revision: snapshot.draft.revision,
      providerId: 'openrouter',
      providerName: 'OpenRouter',
      modelId: snapshot.draft.modelId,
      modelName: 'Fixture model',
      prompt: snapshot.draft.prompt,
      settings: snapshot.draft.settings,
      media: snapshot.draft.media,
      estimatedCost: '$0.20',
      expiresAt: new Date().toISOString()
    };

    const after = applyUiEvent(snapshot, {
      type: 'operation_failed',
      operation: 'submission',
      message: 'Submission outcome is uncertain'
    });

    expect(after.preparedReview).toBeUndefined();
  });

  it('does not let a catalog snapshot erase prompt text waiting for autosave', () => {
    const current = demoSnapshot();
    current.draft = {
      ...current.draft,
      revision: current.draft.revision + 1,
      prompt: 'A hand-drawn moon waves hello from a paper sky.'
    };
    current.draftSaved = false;
    const incoming = demoSnapshot();
    incoming.models = incoming.models.slice().reverse();

    const reconciled = reconcileSnapshot(current, incoming);

    expect(reconciled.draft.prompt).toBe(current.draft.prompt);
    expect(reconciled.draft.revision).toBe(current.draft.revision);
    expect(reconciled.draftSaved).toBe(false);
    expect(reconciled.models).toEqual(incoming.models);
  });

  it('accepts the service save acknowledgement when draft content matches', () => {
    const current = demoSnapshot();
    current.draftSaved = false;
    const incoming = structuredClone(current);
    incoming.draftSaved = true;

    expect(reconcileSnapshot(current, incoming).draftSaved).toBe(true);
  });

  it('preserves a visible local job selection except during an authoritative resync', () => {
    const current = demoSnapshot();
    current.selectedJobId = current.jobs[2].id;
    const incoming = demoSnapshot();
    incoming.selectedJobId = incoming.jobs[1].id;

    expect(reconcileSnapshot(current, incoming).selectedJobId).toBe(current.jobs[2].id);
    expect(
      reconcileSnapshot(current, incoming, { preserveSelection: false }).selectedJobId
    ).toBe(incoming.jobs[1].id);
  });

  it('removes a disconnected provider catalog so stale models cannot be selected', () => {
    const snapshot = demoSnapshot();
    const fal = snapshot.providers.find((provider) => provider.id === 'fal');
    if (!fal) throw new Error('Missing fal fixture');

    const after = applyUiEvent(snapshot, {
      type: 'provider_changed',
      provider: { ...fal, connected: false }
    });

    expect(after.models.some((model) => model.providerId === 'fal')).toBe(false);
    expect(after.models.some((model) => model.providerId === 'openrouter')).toBe(true);
  });

  it('removes a render and selects the next item when the current one is deleted', () => {
    const snapshot = demoSnapshot();
    snapshot.selectedJobId = snapshot.jobs[1].id;

    const after = applyUiEvent(snapshot, {
      type: 'job_removed',
      jobId: snapshot.jobs[1].id
    });

    expect(after.jobs).toHaveLength(snapshot.jobs.length - 1);
    expect(after.jobs.some((job) => job.id === snapshot.jobs[1].id)).toBe(false);
    expect(after.selectedJobId).toBe(after.jobs[0].id);
  });
});
