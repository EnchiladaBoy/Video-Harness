<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { createBridge } from './lib/bridge';
  import { physicalPointToCss, pointInRect } from './lib/geometry';
  import { demoSnapshot } from './lib/mock-bridge';
  import {
    constrainMediaAppend,
    imageRoleOptions,
    modelSupportsImages,
    normalizeDraftForModel,
    reviewReadinessIssue
  } from './lib/readiness';
  import {
    applySequencedEvent,
    reconcileSnapshot,
    requiresImmediateHandling
  } from './lib/state';
  import CloudCinema from './lib/components/CloudCinema.svelte';
  import Icon from './lib/components/Icon.svelte';
  import type {
    AppSnapshot,
    FileDropEvent,
    GenerationDraft,
    JobSummary,
    MediaItem,
    MediaKind,
    MediaRole,
    ModelSummary,
    OpenSessionResult,
    ProviderId,
    UiEventEnvelope,
    VideoHarnessBridge,
    WorkspaceView
  } from './lib/types';
  import { isActivelyMonitored, modelById } from './lib/types';

  let { bridge: bridgeOverride }: { bridge?: VideoHarnessBridge } = $props();
  const bridge = untrack(() => bridgeOverride ?? createBridge());

  function emptySnapshot(): AppSnapshot {
    return {
      providers: [],
      models: [],
      draft: {
        revision: 0,
        providerId: 'openrouter',
        modelId: '',
        prompt: '',
        media: [],
        settings: {
          duration: '',
          resolution: '',
          aspectRatio: '',
          size: '',
          generatedAudio: 'provider_default',
          seed: '',
          advancedJson: ''
        }
      },
      jobs: [],
      draftSaved: true,
      safetyHolds: []
    };
  }

  let snapshot = $state<AppSnapshot>(bridge.mode === 'browser-demo' ? demoSnapshot() : emptySnapshot());
  let activeView = $state<WorkspaceView>('create');
  let ready = $state(bridge.mode === 'browser-demo');
  let sequence = 0;
  let acceptingEvents = false;
  let bufferedEvents: UiEventEnvelope[] = [];
  let resyncInFlight: Promise<void> | undefined;
  let sessionFailed = $state('');
  let isPreparing = $state(false);
  let isSubmitting = $state(false);
  let mediaBusy = $state(false);
  let remoteBusy = $state(false);
  let dropActive = $state(false);
  let showRemoteForm = $state(false);
  let remoteUrl = $state('');
  let remoteKind = $state<MediaKind>('image');
  let remoteRole = $state<MediaRole>('reference');
  let jobSearch = $state('');
  let jobFilter = $state<'all' | 'active' | 'attention' | 'completed'>('all');
  let notice = $state<{ tone: 'neutral' | 'good' | 'warning' | 'danger'; message: string } | null>(null);
  let liveMessage = $state('');
  let providerKeys = $state<Record<ProviderId, string>>({ openrouter: '', fal: '' });
  let providerRemember = $state<Record<ProviderId, boolean>>({ openrouter: true, fal: true });
  let providerBusy = $state<Record<ProviderId, boolean>>({ openrouter: false, fal: false });
  let holdBusy = $state<Record<string, boolean>>({});
  type MonitorAction = 'pause' | 'resume';
  let monitorBusy = $state<Record<string, MonitorAction | undefined>>({});
  let confirmedSafetyHolds = $state<Record<string, boolean>>({});
  let playbackUrl = $state('');
  let playbackReleaseError = $state('');
  let playbackGrantId = '';
  let playbackBusy = $state(false);
  let playbackEpoch = 0;
  let playbackQueue: Promise<void> = Promise.resolve();
  let videoElement = $state<HTMLVideoElement>();
  let jobIdentifierElement = $state<HTMLElement>();
  let outputBusy = $state(false);
  let deleteBusy = $state(false);
  let deleteError = $state('');
  let reviewError = $state('');
  let closeBusy = $state(false);
  let closeCancelBusy = $state(false);
  let closeCancelFailed = $state(false);
  let closeCommitted = $state(false);
  let closeErrorTitle = $state('');
  let closeError = $state('');
  let closeWaitMessage = $state('');
  let closeRequestedAfterSubmission = $state(false);
  let activeCloseRequestId: number | undefined;
  const closeAcknowledgements = new Map<number, Promise<void>>();
  let copiedJobId = $state('');
  let reviewDialog: HTMLDialogElement;
  let uploadDialog: HTMLDialogElement;
  let deleteDialog: HTMLDialogElement;
  let closeDialog: HTMLDialogElement;
  let dropZone = $state<HTMLDivElement>();
  let jobsHeadingElement = $state<HTMLElement>();
  let saveTimer: number | undefined;
  let noticeTimer: number | undefined;
  let copyTimer: number | undefined;
  let announceTimer: number | undefined;
  let latestDraft = $state<GenerationDraft>();
  let saveQueue: Promise<void> = Promise.resolve();
  let pendingSave: { revision: number; promise: Promise<void> } | undefined;
  let selectionEpoch = 0;
  let monitorTimers: Record<string, number> = {};

  let providerModels = $derived(
    snapshot.models.filter((model) => model.providerId === snapshot.draft.providerId)
  );
  let selectedModel = $derived(
    modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId)
  );
  let selectedJob = $derived(snapshot.jobs.find((job) => job.id === snapshot.selectedJobId));
  let selectedJobIdentifier = $derived(selectedJob?.providerJobId ?? selectedJob?.id ?? '');
  let activeJobCount = $derived(snapshot.jobs.filter(isActivelyMonitored).length);
  let requiresOpenRouterUpload = $derived(
    snapshot.draft.providerId === 'openrouter' &&
      snapshot.draft.media.some((item) => item.source === 'local')
  );
  let readinessIssue = $derived(reviewReadinessIssue(snapshot, ready));
  let canReview = $derived(
    ready && !isPreparing && !isSubmitting && readinessIssue === undefined
  );
  function matchingJobs(
    search: string,
    filter: 'all' | 'active' | 'attention' | 'completed'
  ): JobSummary[] {
    const query = search.trim().toLowerCase();
    return snapshot.jobs.filter((job) => {
      const matchesText =
        !query ||
        job.prompt.toLowerCase().includes(query) ||
        job.modelName.toLowerCase().includes(query) ||
        job.providerName.toLowerCase().includes(query) ||
        job.statusLabel.toLowerCase().includes(query) ||
        job.id.toLowerCase().includes(query) ||
        job.providerJobId?.toLowerCase().includes(query);
      const matchesFilter =
        filter === 'all' ||
        (filter === 'active' && isActivelyMonitored(job)) ||
        (filter === 'attention' && (job.status === 'attention' || job.status === 'paused')) ||
        (filter === 'completed' && job.status === 'completed');
      return matchesText && matchesFilter;
    });
  }

  let filteredJobs = $derived(matchingJobs(jobSearch, jobFilter));

  function announce(message: string): void {
    liveMessage = '';
    if (announceTimer) window.clearTimeout(announceTimer);
    announceTimer = window.setTimeout(() => (liveMessage = message), 20);
  }

  function showNotice(
    message: string,
    tone: 'neutral' | 'good' | 'warning' | 'danger' = 'neutral'
  ): void {
    notice = { message, tone };
    if (noticeTimer) window.clearTimeout(noticeTimer);
    noticeTimer = window.setTimeout(() => (notice = null), tone === 'danger' ? 8_000 : 4_500);
  }

  function replayBufferedEvents(): void {
    const replay = bufferedEvents.sort((left, right) => left.seq - right.seq);
    bufferedEvents = [];
    for (const envelope of replay) {
      if (requiresImmediateHandling(envelope.event)) {
        handleCloseRequest(envelope.event.requestId);
      }
      if (!acceptingEvents) bufferedEvents.push(envelope);
      else if (envelope.seq > sequence) consumeEvent(envelope);
    }
  }

  function resyncSnapshot(): Promise<void> {
    if (resyncInFlight) return resyncInFlight;
    acceptingEvents = false;
    resyncInFlight = (async () => {
      try {
        const current = await bridge.getSnapshot();
        const previous = $state.snapshot(snapshot);
        snapshot = reconcileSnapshot(snapshot, current.snapshot, { preserveSelection: false });
        sequence = current.seq;
        reconcileTransientState(previous, snapshot, { operations: current });
      } catch (error) {
        showNotice(errorMessage(error), 'danger');
      } finally {
        // A replay can itself expose a second gap. Release this promise before
        // replaying so that gap starts a fresh authoritative snapshot request.
        resyncInFlight = undefined;
        acceptingEvents = true;
        replayBufferedEvents();
        if (activeCloseRequestId !== undefined && !closeCommitted) {
          handleCloseRequest(activeCloseRequestId);
        }
      }
    })();
    return resyncInFlight;
  }

  function consumeEvent(envelope: UiEventEnvelope): void {
    if (envelope.seq <= sequence) return;
    const previous = $state.snapshot(snapshot);
    const result = applySequencedEvent(snapshot, sequence, envelope);
    snapshot = result.snapshot;
    sequence = result.seq;
    if (result.gap) {
      void resyncSnapshot();
      return;
    }

    const event = envelope.event;
    if (event.type === 'snapshot_changed') {
      // Catalog, history, and safety-state snapshots are routine background
      // updates. They do not authoritatively end an in-flight Review or paid
      // submission, and loading history must not navigate away from Create.
      reconcileTransientState(previous, snapshot, { detectAddedJob: false });
    } else if (event.type === 'review_ready') {
      isPreparing = false;
      if (event.review.revision !== snapshot.draft.revision) {
        snapshot = { ...snapshot, preparedReview: undefined };
        void bridge
          .invalidatePrepared(snapshot.draft.revision)
          .catch((error) => showNotice(errorMessage(error), 'danger'));
        showNotice('That Review belonged to an older edit. Prepare the latest scene instead.', 'warning');
        return;
      }
      reviewError = '';
      announce('Review ready. Nothing paid happens until you press Generate.');
      window.setTimeout(() => reviewDialog?.showModal(), 0);
    } else if (event.type === 'job_added') {
      isSubmitting = false;
      reviewError = '';
      reviewDialog?.close();
      jobSearch = '';
      jobFilter = 'all';
      activeView = 'jobs';
      announce('The provider accepted your request. Tiny Cloud Cinema is keeping watch.');
      void focusJobsHeading();
      resumeDeferredClose();
    } else if (event.type === 'review_invalidated') {
      isPreparing = false;
      isSubmitting = false;
      reviewDialog?.close();
      resumeDeferredClose();
    } else if (event.type === 'operation_failed') {
      if (event.operation === 'preparation') isPreparing = false;
      else {
        isSubmitting = false;
        reviewDialog?.close();
      }
      showNotice(event.message, 'danger');
      resumeDeferredClose();
    } else if (event.type === 'job_updated') {
      settleMonitorBusy(event.job);
      if (event.job.status === 'completed') announce('Your video is ready to watch.');
    } else if (event.type === 'job_removed') {
      clearMonitorBusy(event.jobId);
      deleteDialog?.close();
      deleteError = '';
      void ensureVisibleJobSelection();
    } else if (event.type === 'draft_saved') {
      if (latestDraft?.revision === event.revision && snapshot.draft.revision === event.revision) {
        latestDraft = undefined;
      }
    } else if (event.type === 'notice') {
      if (closeDialog?.open && (closeBusy || closeCommitted)) {
        closeWaitMessage = event.message;
      }
      showNotice(event.message, event.tone);
    } else if (event.type === 'close_requested') {
      handleCloseRequest(event.requestId);
    }
  }

  function receiveEvent(envelope: UiEventEnvelope): void {
    if (requiresImmediateHandling(envelope.event)) {
      const canActImmediately = acceptingEvents && envelope.seq === sequence + 1;
      handleCloseRequest(envelope.event.requestId, canActImmediately);
    }
    if (!acceptingEvents) bufferedEvents.push(envelope);
    else consumeEvent(envelope);
  }

  function acknowledgeCloseRequest(requestId: number): Promise<void> {
    const existing = closeAcknowledgements.get(requestId);
    if (existing) return existing;
    const operation = bridge.acknowledgeCloseRequest(requestId);
    closeAcknowledgements.set(requestId, operation);
    void operation.then(
      () => undefined,
      () => {
        if (closeAcknowledgements.get(requestId) === operation) {
          closeAcknowledgements.delete(requestId);
        }
      }
    );
    return operation;
  }

  function handleCloseRequest(requestId: number, allowSave = true): void {
    void acknowledgeCloseRequest(requestId).catch(() => undefined);
    if (closeCommitted) return;
    // Native request IDs increase for each completed/cancelled close cycle.
    // Keep the newest edge even while an older cancellation promise is still
    // settling; otherwise its acknowledged watchdog would have no UI owner.
    if (activeCloseRequestId === undefined || requestId >= activeCloseRequestId) {
      activeCloseRequestId = requestId;
    }
    if (activeCloseRequestId !== requestId || !allowSave || !ready) return;
    void beginCloseSave();
  }

  function reconcileTransientState(
    previous: AppSnapshot,
    current: AppSnapshot,
    options: {
      detectAddedJob?: boolean;
      operations?: Pick<OpenSessionResult, 'preparing' | 'submitting'>;
    } = {}
  ): void {
    const review = current.preparedReview;
    const reviewIsCurrent = Boolean(review && review.revision === current.draft.revision);
    if (options.operations) {
      isPreparing = options.operations.preparing;
      isSubmitting = options.operations.submitting;
      if (isPreparing || !reviewIsCurrent) {
        reviewError = '';
        reviewDialog?.close();
      } else if (review) {
        const reviewChanged = previous.preparedReview?.preparedId !== review.preparedId;
        if (reviewChanged && !isPreparing && !isSubmitting) {
          announce('Review restored after refreshing the desktop session.');
          window.setTimeout(() => {
            if (!reviewDialog?.open) reviewDialog?.showModal();
          }, 0);
        }
      }
    } else if (
      previous.preparedReview &&
      !reviewIsCurrent &&
      !isSubmitting
    ) {
      reviewError = '';
      reviewDialog?.close();
    }

    const previousIds = new Set(previous.jobs.map((job) => job.id));
    const addedJob =
      options.detectAddedJob === false
        ? undefined
        : current.jobs.find((job) => !previousIds.has(job.id));
    if (addedJob) {
      isSubmitting = false;
      jobSearch = '';
      jobFilter = 'all';
      activeView = 'jobs';
      announce('A render was recovered after refreshing the desktop session.');
      void focusJobsHeading();
    }

    if (
      deleteDialog?.open &&
      previous.selectedJobId &&
      !current.jobs.some((job) => job.id === previous.selectedJobId)
    ) {
      deleteBusy = false;
      deleteError = '';
      deleteDialog.close();
    }

    for (const jobId of Object.keys(monitorBusy)) {
      const after = current.jobs.find((job) => job.id === jobId);
      if (!after) {
        clearMonitorBusy(jobId);
      } else {
        settleMonitorBusy(after);
      }
    }
    void ensureVisibleJobSelection();
  }

  async function focusJobsHeading(): Promise<void> {
    await tick();
    jobsHeadingElement?.focus();
  }

  onMount(() => {
    let disposed = false;
    let dropSubscription: { close(): void } | undefined;

    void bridge
      .openSession(receiveEvent)
      .then((session) => {
        if (disposed) return;
        const previous = $state.snapshot(snapshot);
        snapshot = reconcileSnapshot(snapshot, session.snapshot, { preserveSelection: false });
        sequence = session.seq;
        acceptingEvents = true;
        ready = true;
        reconcileTransientState(previous, snapshot, {
          detectAddedJob: false,
          operations: session
        });
        replayBufferedEvents();
        if (activeCloseRequestId !== undefined && !closeCommitted) {
          handleCloseRequest(activeCloseRequestId);
        }
      })
      .catch((error) => {
        sessionFailed = errorMessage(error);
        ready = false;
        if (activeCloseRequestId !== undefined) {
          closeErrorTitle = 'The draft could not be loaded safely.';
          closeError =
            'Video Harness acknowledged the close request but stayed open. Choose Keep working to cancel it.';
          window.setTimeout(() => closeDialog?.showModal(), 0);
        }
      });

    void bridge
      .watchFileDrops(handleNativeDrop)
      .then((subscription) => {
        if (disposed) subscription.close();
        else dropSubscription = subscription;
      })
      .catch((error) => {
        if (!disposed) {
          showNotice(`File dropping is unavailable. ${errorMessage(error)} Use Choose files instead.`, 'warning');
        }
      });

    const flushForLifecycle = () => {
      if (!snapshot.draftSaved) void flushLatestDraft(false).catch(() => undefined);
    };
    window.addEventListener('pagehide', flushForLifecycle);
    window.addEventListener('blur', flushForLifecycle);

    return () => {
      disposed = true;
      dropSubscription?.close();
      window.removeEventListener('pagehide', flushForLifecycle);
      window.removeEventListener('blur', flushForLifecycle);
      if (!snapshot.draftSaved) void flushLatestDraft(false).catch(() => undefined);
      else if (saveTimer) window.clearTimeout(saveTimer);
      if (noticeTimer) window.clearTimeout(noticeTimer);
      if (copyTimer) window.clearTimeout(copyTimer);
      if (announceTimer) window.clearTimeout(announceTimer);
      for (const timer of Object.values(monitorTimers)) window.clearTimeout(timer);
      playbackEpoch += 1;
      void releasePlayback();
    };
  });

  function errorMessage(error: unknown): string {
    if (error instanceof Error) return error.message;
    return typeof error === 'string' ? error : 'Something unexpected happened.';
  }

  function setView(view: WorkspaceView): void {
    activeView = view;
    if (view !== 'jobs') {
      playbackEpoch += 1;
      void releasePlayback();
    }
  }

  function queueDraftSave(draft: GenerationDraft, reportErrors = true): Promise<void> {
    if (pendingSave?.revision === draft.revision) return pendingSave.promise;
    const payload = cloneDraft(draft);
    const operation = saveQueue.then(() => bridge.saveDraft(payload));
    saveQueue = operation.then(
      () => undefined,
      () => undefined
    );
    pendingSave = { revision: payload.revision, promise: operation };
    void operation.then(
      () => {
        if (pendingSave?.promise === operation) pendingSave = undefined;
      },
      (error) => {
        if (pendingSave?.promise === operation) pendingSave = undefined;
        if (reportErrors) showNotice(errorMessage(error), 'danger');
      }
    );
    return operation;
  }

  function scheduleDraftSave(draft: GenerationDraft): void {
    latestDraft = cloneDraft(draft);
    if (saveTimer) window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      saveTimer = undefined;
      void queueDraftSave(draft).catch(() => undefined);
    }, 650);
  }

  function flushLatestDraft(reportErrors = true): Promise<void> {
    if (saveTimer) {
      window.clearTimeout(saveTimer);
      saveTimer = undefined;
    }
    const draft = latestDraft ?? draftSnapshot();
    if (snapshot.draftSaved && !latestDraft) return saveQueue;
    return queueDraftSave(draft, reportErrors);
  }

  async function beginCloseSave(): Promise<void> {
    if (closeCommitted || closeCancelBusy) return;
    const requestId = activeCloseRequestId;
    if (requestId === undefined) return;
    if (isSubmitting) {
      closeRequestedAfterSubmission = true;
      announce('Close requested. Waiting for the paid submission outcome first.');
      return;
    }
    uploadDialog?.close();
    deleteDialog?.close();
    reviewDialog?.close();
    if (!closeDialog?.open) closeDialog?.showModal();
    if (closeBusy) return;
    closeBusy = true;
    closeCancelFailed = false;
    closeErrorTitle = '';
    closeError = '';
    closeWaitMessage = '';
    if (saveTimer) {
      window.clearTimeout(saveTimer);
      saveTimer = undefined;
    }
    const closingDraft = cloneDraft(latestDraft ?? draftSnapshot());
    try {
      try {
        await acknowledgeCloseRequest(requestId);
      } catch {
        closeErrorTitle = 'The close request could not be confirmed.';
        closeError =
          'Video Harness stayed open. Retry save and close, or choose Keep working.';
        return;
      }
      await saveQueue;
      await bridge.saveDraftAndClose(closingDraft, requestId);
      if (bridge.mode === 'browser-demo') {
        closeBusy = false;
        activeCloseRequestId = undefined;
        closeAcknowledgements.delete(requestId);
        closeDialog.close();
        showNotice('Demo draft saved. The browser tab stays open in demo mode.', 'good');
      } else {
        closeCommitted = true;
      }
    } catch (error) {
      closeErrorTitle = 'The draft wasn’t saved, so the app stayed open.';
      closeError = errorMessage(error);
    } finally {
      if (!closeCommitted) closeBusy = false;
    }
  }

  async function cancelCloseRequest(): Promise<void> {
    if (closeBusy || closeCancelBusy || closeCommitted) return;
    const requestId = activeCloseRequestId;
    if (requestId === undefined) {
      closeDialog.close();
      return;
    }
    closeCancelBusy = true;
    closeCancelFailed = false;
    closeErrorTitle = '';
    closeError = '';
    try {
      await bridge.cancelCloseRequest(requestId);
      closeAcknowledgements.delete(requestId);
      closeRequestedAfterSubmission = false;
      closeWaitMessage = '';
      if (activeCloseRequestId === requestId) {
        activeCloseRequestId = undefined;
        closeDialog.close();
      }
    } catch {
      if (activeCloseRequestId === requestId) {
        closeCancelFailed = true;
        closeErrorTitle = 'The close request is still active.';
        closeError =
          'Video Harness could not safely cancel it. Retry Keep working, or save and close.';
      }
    } finally {
      closeCancelBusy = false;
      if (
        activeCloseRequestId !== undefined &&
        activeCloseRequestId !== requestId &&
        !closeCommitted
      ) {
        handleCloseRequest(activeCloseRequestId);
      }
    }
  }

  function resumeDeferredClose(): void {
    if (!closeRequestedAfterSubmission || isSubmitting) return;
    closeRequestedAfterSubmission = false;
    window.setTimeout(() => void beginCloseSave(), 0);
  }

  function draftSnapshot(): GenerationDraft {
    // Svelte 5 deep state is a Proxy and cannot be passed to
    // structuredClone. Snapshotting produces the plain, serializable draft
    // expected by the Rust bridge and local immutable edits.
    return $state.snapshot(snapshot.draft);
  }

  function cloneDraft(draft: GenerationDraft): GenerationDraft {
    return {
      ...draft,
      media: draft.media.map((item) => ({ ...item })),
      settings: { ...draft.settings }
    };
  }

  function editDraft(change: (draft: GenerationDraft) => void): void {
    const hadReview = Boolean(snapshot.preparedReview);
    const hadActivePreparation = isPreparing;
    const draft = draftSnapshot();
    change(draft);
    draft.revision += 1;
    snapshot = { ...snapshot, draft, preparedReview: undefined, draftSaved: false };
    latestDraft = cloneDraft(draft);
    if (hadReview || hadActivePreparation) {
      isPreparing = false;
      reviewDialog?.close();
      void bridge
        .invalidatePrepared(draft.revision)
        .catch((error) => showNotice(errorMessage(error), 'danger'));
    }
    scheduleDraftSave(draft);
  }

  function changeProvider(providerId: ProviderId): void {
    const firstModel = snapshot.models.find((model) => model.providerId === providerId);
    editDraft((draft) => {
      draft.providerId = providerId;
      draft.modelId = firstModel?.id ?? '';
      normalizeDraftForModel(draft, firstModel, { chooseDefaults: true, clearAdvanced: true });
    });
    normalizeRemoteControls(firstModel);
  }

  function changeModel(modelId: string): void {
    const model = providerModels.find((item) => item.id === modelId);
    editDraft((draft) => {
      draft.modelId = modelId;
      normalizeDraftForModel(draft, model, { chooseDefaults: true, clearAdvanced: true });
    });
    normalizeRemoteControls(model);
  }

  function normalizeRemoteControls(model: ModelSummary | undefined): void {
    const supportsCurrent =
      !model ||
      (remoteKind === 'image' && modelSupportsImages(model)) ||
      (remoteKind === 'video' && model.capabilities.video) ||
      (remoteKind === 'audio' && model.capabilities.audioReferences);
    if (!supportsCurrent && model) {
      remoteKind = modelSupportsImages(model)
        ? 'image'
        : model.capabilities.video
          ? 'video'
          : model.capabilities.audioReferences
            ? 'audio'
            : 'image';
    }
    if (remoteKind === 'video') remoteRole = 'video_reference';
    else if (remoteKind === 'audio') remoteRole = 'audio_reference';
    else {
      const roles = imageRoleOptions(model);
      if (!roles.some((option) => option.value === remoteRole)) {
        remoteRole = roles[0]?.value ?? 'reference';
      }
    }
  }

  function changeResolution(value: string): void {
    editDraft((draft) => {
      draft.settings.resolution = value;
      if (value) draft.settings.size = '';
    });
  }

  function changeAspectRatio(value: string): void {
    editDraft((draft) => {
      draft.settings.aspectRatio = value;
      if (value) draft.settings.size = '';
    });
  }

  function changeOutputSize(value: string): void {
    editDraft((draft) => {
      draft.settings.size = value;
      if (value) {
        draft.settings.resolution = '';
        draft.settings.aspectRatio = '';
      }
    });
  }

  function appendMedia(items: MediaItem[]): number {
    if (items.length === 0) return 0;
    const { accepted, skipped } = constrainMediaAppend(
      snapshot.draft.media,
      items,
      selectedModel
    );
    if (accepted.length > 0) editDraft((draft) => draft.media.push(...accepted));
    if (skipped > 0) {
      showNotice(
        accepted.length > 0
          ? `${accepted.length} added; ${skipped} skipped to stay within this model’s media limits.`
          : 'No references were added because this model or draft has no room for them.',
        'warning'
      );
    } else {
      showNotice(
        `${accepted.length} ${accepted.length === 1 ? 'reference' : 'references'} tucked in.`,
        'good'
      );
    }
    return accepted.length;
  }

  async function chooseMedia(): Promise<void> {
    mediaBusy = true;
    try {
      appendMedia(await bridge.chooseMedia());
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      mediaBusy = false;
    }
  }

  async function attachPaths(paths: string[]): Promise<void> {
    if (paths.length === 0) return;
    mediaBusy = true;
    try {
      appendMedia(await bridge.attachDroppedMedia(paths));
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      mediaBusy = false;
      dropActive = false;
    }
  }

  function pointIsInDropZone(position?: { x: number; y: number }): boolean {
    if (!position || !dropZone) return true;
    const bounds = dropZone.getBoundingClientRect();
    // Tauri reports drag positions in physical pixels while DOM geometry is
    // measured in CSS pixels. devicePixelRatio follows the WebView's current
    // monitor and zoom, including mixed-DPI desktop setups.
    const point = physicalPointToCss(
      position,
      bridge.mode === 'tauri' ? window.devicePixelRatio : 1
    );
    return pointInRect(point, bounds);
  }

  function handleNativeDrop(event: FileDropEvent): void {
    if (event.type === 'over') dropActive = pointIsInDropZone(event.position);
    else if (event.type === 'drop') {
      if (pointIsInDropZone(event.position)) void attachPaths(event.paths);
      else dropActive = false;
    } else dropActive = false;
  }

  function handleBrowserDrop(event: DragEvent): void {
    event.preventDefault();
    dropActive = false;
    if (bridge.mode !== 'browser-demo') return;
    const names = Array.from(event.dataTransfer?.files ?? []).map((file) => file.name);
    void attachPaths(names);
  }

  async function addRemoteReference(): Promise<void> {
    if (remoteBusy) return;
    remoteBusy = true;
    try {
      const role =
        remoteKind === 'video'
          ? 'video_reference'
          : remoteKind === 'audio'
            ? 'audio_reference'
            : remoteRole;
      const item = await bridge.addRemoteMedia(remoteUrl.trim(), remoteKind, role);
      if (appendMedia([item]) > 0) {
        remoteUrl = '';
        showRemoteForm = false;
      }
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      remoteBusy = false;
    }
  }

  function removeMedia(handle: string): void {
    editDraft((draft) => {
      draft.media = draft.media.filter((item) => item.handle !== handle);
    });
  }

  function moveMedia(index: number, direction: -1 | 1): void {
    editDraft((draft) => {
      const nextIndex = index + direction;
      if (nextIndex < 0 || nextIndex >= draft.media.length) return;
      const [item] = draft.media.splice(index, 1);
      draft.media.splice(nextIndex, 0, item);
    });
  }

  function changeMediaRole(handle: string, role: MediaRole): void {
    editDraft((draft) => {
      const item = draft.media.find((candidate) => candidate.handle === handle);
      if (item?.kind === 'image') item.role = role;
    });
  }

  function beginReview(): void {
    if (!canReview) return;
    if (requiresOpenRouterUpload) uploadDialog.showModal();
    else void prepareReview(false);
  }

  async function prepareReview(localMediaUploadConfirmed: boolean): Promise<void> {
    uploadDialog?.close();
    isPreparing = true;
    reviewError = '';
    announce('Preparing your Review and a fresh price estimate.');
    if (saveTimer) {
      window.clearTimeout(saveTimer);
      saveTimer = undefined;
    }
    const reviewedDraft = draftSnapshot();
    try {
      // Wait for the exact revision to be durably acknowledged before
      // preparation. A delayed SaveDraft must never arrive behind
      // PrepareGeneration and cancel its active work.
      await queueDraftSave(reviewedDraft, false);
      if (snapshot.draft.revision !== reviewedDraft.revision) {
        isPreparing = false;
        showNotice('The scene changed while saving. Review the latest take instead.', 'warning');
        return;
      }
      await bridge.prepareGeneration(reviewedDraft, { localMediaUploadConfirmed });
    } catch (error) {
      isPreparing = false;
      showNotice(errorMessage(error), 'danger');
    }
  }

  async function submitReview(): Promise<void> {
    const review = snapshot.preparedReview;
    if (!review || isSubmitting) return;
    isSubmitting = true;
    reviewError = '';
    try {
      await bridge.submitPrepared(review.preparedId);
    } catch (error) {
      isSubmitting = false;
      reviewError = errorMessage(error);
      resumeDeferredClose();
    }
  }

  function closeReview(): void {
    if (isSubmitting) {
      reviewError = 'The paid request is already being submitted. Keep this window open until its outcome is known.';
      return;
    }
    reviewError = '';
    reviewDialog.close();
  }

  async function connectProvider(providerId: ProviderId): Promise<void> {
    if (providerBusy[providerId]) return;
    const key = providerKeys[providerId].trim();
    if (!key) {
      showNotice('Paste a key first.', 'warning');
      return;
    }
    providerKeys[providerId] = '';
    providerBusy[providerId] = true;
    try {
      await bridge.connectProvider(providerId, key, providerRemember[providerId]);
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      providerBusy[providerId] = false;
    }
  }

  async function forgetProvider(providerId: ProviderId): Promise<void> {
    if (providerBusy[providerId]) return;
    providerBusy[providerId] = true;
    try {
      await bridge.forgetProvider(providerId);
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      providerBusy[providerId] = false;
    }
  }

  async function acknowledgeSafetyHold(handle: string): Promise<void> {
    if (!confirmedSafetyHolds[handle] || holdBusy[handle]) return;
    holdBusy[handle] = true;
    try {
      await bridge.acknowledgeSafetyHold(handle);
      showNotice('Dashboard check sent. You can Review this exact request once the hold clears.', 'good');
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      holdBusy[handle] = false;
    }
  }

  async function selectJob(jobId: string | undefined): Promise<void> {
    const requestEpoch = ++selectionEpoch;
    if (snapshot.selectedJobId !== jobId) {
      playbackEpoch += 1;
      await releasePlayback();
    }
    if (requestEpoch !== selectionEpoch) return;
    snapshot = { ...snapshot, selectedJobId: jobId };
  }

  function ensureVisibleJobSelection(
    search = jobSearch,
    filter: 'all' | 'active' | 'attention' | 'completed' = jobFilter
  ): Promise<void> {
    const visible = matchingJobs(search, filter);
    const selectedIsVisible = visible.some((job) => job.id === snapshot.selectedJobId);
    return selectedIsVisible ? Promise.resolve() : selectJob(visible[0]?.id);
  }

  function updateJobSearch(value: string): void {
    jobSearch = value;
    void ensureVisibleJobSelection(value, jobFilter);
  }

  function updateJobFilter(value: 'all' | 'active' | 'attention' | 'completed'): void {
    jobFilter = value;
    void ensureVisibleJobSelection(jobSearch, value);
  }

  function canResumeMonitoring(job: JobSummary): boolean {
    return job.canResume ?? job.status === 'paused';
  }

  function canPauseMonitoring(job: JobSummary): boolean {
    return job.canPause ?? isActivelyMonitored(job);
  }

  function clearMonitorBusy(jobId: string): void {
    delete monitorBusy[jobId];
    if (monitorTimers[jobId]) window.clearTimeout(monitorTimers[jobId]);
    delete monitorTimers[jobId];
  }

  function settleMonitorBusy(job: JobSummary): void {
    const action = monitorBusy[job.id];
    if (!action) return;
    const terminal = job.monitorState === 'terminal';
    const reachedTarget =
      action === 'pause'
        ? canResumeMonitoring(job)
        : job.monitorState === 'active' && canPauseMonitoring(job);
    if (terminal || reachedTarget) clearMonitorBusy(job.id);
  }

  async function toggleJobMonitoring(): Promise<void> {
    const job = selectedJob;
    if (!job || monitorBusy[job.id]) return;
    const resume = canResumeMonitoring(job);
    if (!resume && !canPauseMonitoring(job)) return;
    monitorBusy[job.id] = resume ? 'resume' : 'pause';
    monitorTimers[job.id] = window.setTimeout(() => {
      clearMonitorBusy(job.id);
      showNotice('The monitoring change is taking longer than expected. Check the latest job status.', 'warning');
    }, 15_000);
    try {
      if (resume) await bridge.resumeJob(job.id);
      else await bridge.pauseJob(job.id);
    } catch (error) {
      clearMonitorBusy(job.id);
      showNotice(errorMessage(error), 'danger');
    }
  }

  function withPlaybackLock<T>(operation: () => Promise<T>): Promise<T> {
    const result = playbackQueue.then(operation, operation);
    playbackQueue = result.then(
      () => undefined,
      () => undefined
    );
    return result;
  }

  async function releaseGrant(grantId: string): Promise<boolean> {
    playbackReleaseError = '';
    for (let attempt = 0; attempt < 5; attempt += 1) {
      try {
        await bridge.releasePlayback(grantId);
        return true;
      } catch (error) {
        if (attempt === 4) {
          playbackReleaseError = errorMessage(error);
          showNotice(
            `${errorMessage(error)} The private playback cache will be cleaned on the next launch.`,
            'warning'
          );
          return false;
        }
        await new Promise<void>((resolve) =>
          window.setTimeout(resolve, 100 * (attempt + 1))
        );
      }
    }
    return false;
  }

  async function releasePlaybackNow(): Promise<boolean> {
    const grant = playbackGrantId;
    playbackUrl = '';
    videoElement?.pause();
    videoElement?.removeAttribute('src');
    videoElement?.load();
    await tick();
    if (!grant) return true;

    const released = await releaseGrant(grant);
    if (released && playbackGrantId === grant) playbackGrantId = '';
    return released;
  }

  function releasePlayback(): Promise<boolean> {
    return withPlaybackLock(releasePlaybackNow);
  }

  async function loadPlayback(): Promise<boolean> {
    const job = selectedJob;
    if (!job || job.status !== 'completed' || !job.hasLocalOutput || playbackBusy) return false;
    const requestEpoch = playbackEpoch;
    playbackBusy = true;
    try {
      return await withPlaybackLock(async () => {
        if (!(await releasePlaybackNow())) return false;
        try {
          const grant = await bridge.grantPlayback(job.id);
          if (requestEpoch !== playbackEpoch || snapshot.selectedJobId !== job.id) {
            playbackGrantId = grant.grantId;
            const released = await releaseGrant(grant.grantId);
            if (released && playbackGrantId === grant.grantId) playbackGrantId = '';
            return false;
          }
          playbackGrantId = grant.grantId;
          playbackUrl = grant.url;
          if (!grant.url && bridge.mode === 'browser-demo') {
            showNotice('This little demo has no video file to play.', 'neutral');
          }
          return Boolean(grant.url);
        } catch (error) {
          showNotice(errorMessage(error), 'danger');
          return false;
        }
      });
    } finally {
      playbackBusy = false;
    }
  }

  async function playSelectedOutput(): Promise<void> {
    if (!(await loadPlayback())) return;
    await tick();
    videoElement?.scrollIntoView({ behavior: 'smooth', block: 'center' });
    try {
      const playback = videoElement?.play();
      if (playback) await playback;
    } catch {
      showNotice('Your film is loaded—press play when you’re ready.', 'neutral');
    }
  }

  async function openSelectedOutput(): Promise<void> {
    if (!selectedJob || selectedJob.status !== 'completed' || !selectedJob.hasLocalOutput || outputBusy) return;
    outputBusy = true;
    try {
      await bridge.openOutput(selectedJob.id);
      showNotice('Handed off to your system player. If it stays quiet, use Play here.', 'neutral');
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      outputBusy = false;
    }
  }

  async function copyJobIdentifier(value: string, jobId: string): Promise<void> {
    let copied = false;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(value);
        copied = true;
      }
    } catch {
      // Some WebViews expose the Clipboard API but deny it. Try the compatible fallback below.
    }

    if (!copied) {
      const textarea = document.createElement('textarea');
      textarea.value = value;
      textarea.readOnly = true;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.appendChild(textarea);
      try {
        textarea.select();
        copied = document.execCommand('copy');
      } catch {
        copied = false;
      } finally {
        textarea.remove();
      }
    }

    if (!copied) {
      if (snapshot.selectedJobId === jobId && jobIdentifierElement) {
        const range = document.createRange();
        range.selectNodeContents(jobIdentifierElement);
        const selection = window.getSelection();
        selection?.removeAllRanges();
        selection?.addRange(range);
        jobIdentifierElement.focus();
        showNotice('Full ID selected—press Ctrl+C to copy it.', 'warning');
      } else {
        showNotice('Clipboard access is unavailable. Select the full ID and press Ctrl+C.', 'warning');
      }
      return;
    }

    copiedJobId = jobId;
    showNotice('Job identifier copied.', 'good');
    if (copyTimer) window.clearTimeout(copyTimer);
    copyTimer = window.setTimeout(() => (copiedJobId = ''), 2_000);
  }

  function askToDeleteRender(): void {
    if (!selectedJob?.deletable) return;
    deleteError = '';
    deleteDialog.showModal();
  }

  async function deleteSelectedRender(deleteOutput: boolean): Promise<void> {
    const job = selectedJob;
    if (!job?.deletable || deleteBusy) return;
    deleteBusy = true;
    try {
      playbackEpoch += 1;
      if (!(await releasePlayback())) {
        deleteError = playbackReleaseError || 'The in-app video could not be stopped safely.';
        return;
      }
      await bridge.deleteRender(job.id, deleteOutput);
      deleteDialog.close();
      announce('Render removed from your reel.');
      showNotice(
        deleteOutput ? 'Render and saved video deleted.' : 'Render cleared from your reel.',
        'good'
      );
    } catch (error) {
      deleteError = errorMessage(error);
    } finally {
      deleteBusy = false;
    }
  }

  function mediaRoleLabel(role: MediaRole): string {
    const labels: Record<MediaRole, string> = {
      reference: 'Reference',
      start_frame: 'Start frame',
      end_frame: 'End frame',
      video_reference: 'Video reference',
      audio_reference: 'Audio reference'
    };
    return labels[role];
  }

  function statusTone(job: JobSummary): string {
    if (job.status === 'completed') return 'good';
    if (job.status === 'attention') return 'danger';
    if (job.monitorState === 'paused' || job.monitorState === 'recoverable' || job.status === 'paused') {
      return 'warning';
    }
    return 'active';
  }

  function formatDate(value: string): string {
    return new Intl.DateTimeFormat(undefined, {
      month: 'short',
      day: 'numeric',
      hour: 'numeric',
      minute: '2-digit'
    }).format(new Date(value));
  }

  function compatibilityMessage(): string {
    if (isSubmitting) return 'Submitting the paid request once…';
    if (isPreparing) return 'Preparing a fresh Review…';
    return readinessIssue ?? 'All set for Review.';
  }
</script>

<svelte:head><title>Video Harness</title></svelte:head>

<div class="app-shell">
  <header class="topbar">
    <button class="brand" aria-label="Video Harness home" onclick={() => setView('create')}>
      <span class="brand__mark" aria-hidden="true"><span></span><span></span></span>
      <span class="brand__words"><strong>Video Harness</strong><small>Tiny movie studio</small></span>
    </button>

    <nav class="primary-nav" aria-label="Workspace">
      <button class:active={activeView === 'create'} aria-current={activeView === 'create' ? 'page' : undefined} onclick={() => setView('create')}>
        <Icon name="spark" size={17} /><span>Create</span>
      </button>
      <button class:active={activeView === 'jobs'} aria-current={activeView === 'jobs' ? 'page' : undefined} onclick={() => setView('jobs')}>
        <Icon name="film" size={17} /><span>Renders</span>
        {#if activeJobCount > 0}<span class="nav-count" aria-label={`${activeJobCount} active jobs`}>{activeJobCount}</span>{/if}
      </button>
      <button class:active={activeView === 'providers'} aria-current={activeView === 'providers' ? 'page' : undefined} onclick={() => setView('providers')}>
        <Icon name="plug" size={17} /><span>Providers</span>
      </button>
    </nav>

    <div class="topbar__status">
      <span class:online={ready} class="connection-dot" aria-hidden="true"></span>
      <span>{ready ? 'Ready when you are' : 'Warming up…'}</span>
    </div>
  </header>

  {#if bridge.mode === 'browser-demo'}
    <aside class="demo-banner" aria-label="Browser demo mode">
      <span class="demo-banner__pixel" aria-hidden="true">◆</span>
      <p><strong>Demo mode</strong> — everything here is make-believe. Nothing is uploaded, sent, or billed.</p>
    </aside>
  {/if}

  {#if sessionFailed}
    <main class="fatal-state">
      <div class="empty-illustration" aria-hidden="true"><Icon name="warning" size={32} /></div>
      <h1>Video Harness couldn’t open</h1>
      <p>{sessionFailed}</p>
      <button class="button button--primary" onclick={() => window.location.reload()}>Try again</button>
    </main>
  {:else if activeView === 'create'}
    <main class="workspace create-page">
      <section class="page-heading">
        <div>
          <p class="eyebrow">New scene</p>
          <h1>Make a little movie magic.</h1>
          <p>Set the scene, add a clue or two, then check everything before the paid part.</p>
        </div>
        <span class:unsaved={!snapshot.draftSaved} class="save-state">
          <span aria-hidden="true">{snapshot.draftSaved ? '●' : '○'}</span>
          {snapshot.draftSaved ? 'Saved on this device' : 'Saving…'}
        </span>
      </section>

      <div class="compose-layout">
        <div class="compose-main">
          <section class="card prompt-card" aria-labelledby="prompt-heading">
            <div class="card-heading">
              <div>
                <p class="step-label">01 / THE SCENE</p>
                <h2 id="prompt-heading">Set the scene.</h2>
              </div>
              <span class="character-count">{snapshot.draft.prompt.length.toLocaleString()} / 8,000</span>
            </div>
            <label class="sr-only" for="generation-prompt">Video prompt</label>
            <textarea
              id="generation-prompt"
              rows="7"
              maxlength="8000"
              disabled={!ready}
              placeholder="A tiny night train rolls through clouds, its windows glowing like fireflies…"
              bind:value={snapshot.draft.prompt}
              oninput={(event) => editDraft((draft) => (draft.prompt = event.currentTarget.value))}
              onblur={() => void flushLatestDraft().catch(() => undefined)}
            ></textarea>
            <div class="prompt-footnote">
              <p>Motion, light, framing, pace—the little things make the shot.</p>
            </div>
          </section>

          <section class="card media-card" aria-labelledby="media-heading">
            <div class="card-heading">
              <div>
                <p class="step-label">02 / LITTLE CLUES</p>
                <h2 id="media-heading">Bring a few clues.</h2>
                <p>Guide the shot with a frame, clip, or sound.</p>
              </div>
              {#if snapshot.draft.media.length > 0}<span class="count-pill">{snapshot.draft.media.length}</span>{/if}
            </div>

            <div
              class:dragging={dropActive}
              class="drop-zone"
              role="group"
              aria-label="Reference media drop area"
              bind:this={dropZone}
              ondragenter={(event) => { event.preventDefault(); if (bridge.mode === 'browser-demo') dropActive = true; }}
              ondragover={(event) => event.preventDefault()}
              ondragleave={() => { if (bridge.mode === 'browser-demo') dropActive = false; }}
              ondrop={handleBrowserDrop}
            >
              <div class="drop-zone__icon" aria-hidden="true"><Icon name="paperclip" size={24} /></div>
              <div>
                <strong>{dropActive ? 'That’s it—drop them here' : 'Drop a frame, clip, or sound'}</strong>
                <p>Nothing leaves your device until you approve an upload in Review.</p>
              </div>
              <button class="button button--secondary" disabled={mediaBusy} onclick={chooseMedia}>
                <Icon name="plus" size={16} /> {mediaBusy ? 'Checking…' : 'Choose files'}
              </button>
            </div>

            <div class="reference-tools">
              <button class="text-button" aria-expanded={showRemoteForm} aria-controls="remote-reference-form" onclick={() => (showRemoteForm = !showRemoteForm)}>
                <span aria-hidden="true">＋</span> Or add a public link
              </button>
            </div>

            {#if showRemoteForm}
              <form id="remote-reference-form" class="remote-form" aria-busy={remoteBusy} onsubmit={(event) => { event.preventDefault(); void addRemoteReference(); }}>
                <label class="field field--wide">
                  <span>Public HTTPS URL</span>
                  <input bind:value={remoteUrl} required type="url" pattern="https://.*" placeholder="https://example.com/reference.mp4" />
                </label>
                <label class="field">
                  <span>Media type</span>
                  <select bind:value={remoteKind} onchange={() => {
                    remoteRole = remoteKind === 'video'
                      ? 'video_reference'
                      : remoteKind === 'audio'
                        ? 'audio_reference'
                        : imageRoleOptions(selectedModel)[0]?.value ?? 'reference';
                  }}>
                    <option value="image" disabled={selectedModel ? !modelSupportsImages(selectedModel) : false}>Image</option>
                    <option value="video" disabled={selectedModel ? !selectedModel.capabilities.video : false}>Video</option>
                    <option value="audio" disabled={selectedModel ? !selectedModel.capabilities.audioReferences : false}>Audio</option>
                  </select>
                </label>
                {#if remoteKind === 'image'}
                  <label class="field">
                    <span>Role</span>
                    <select bind:value={remoteRole}>
                      {#each imageRoleOptions(selectedModel) as option (option.value)}
                        <option value={option.value}>{option.label}</option>
                      {/each}
                    </select>
                  </label>
                {:else}
                  <div class="fixed-role"><span>Role</span><strong>{remoteKind === 'video' ? 'Video reference' : 'Audio reference'}</strong></div>
                {/if}
                <button class="button button--secondary" disabled={remoteBusy} type="submit">{remoteBusy ? 'Adding…' : 'Add URL'}</button>
              </form>
            {/if}

            {#if snapshot.draft.media.length > 0}
              <ol class="media-list" aria-label="Ordered reference media">
                {#each snapshot.draft.media as item, index (item.handle)}
                  {@const supportedRoles = imageRoleOptions(selectedModel)}
                  <li class="media-item">
                    <div class={`media-thumb media-thumb--${item.kind}`}>
                      {#if item.previewUrl}<img src={item.previewUrl} alt="" />{:else}<Icon name={item.kind} size={21} />{/if}
                    </div>
                    <div class="media-item__name">
                      <strong title={item.displayName}>{item.displayName}</strong>
                      <span>{item.detail} · {item.source === 'local' ? 'Local' : 'HTTPS'}</span>
                      {#if item.displayUrl}<small class="media-item__url" title={item.displayUrl}>{item.displayUrl}</small>{/if}
                    </div>
                    {#if item.kind === 'image'}
                      <label class="compact-field">
                        <span class="sr-only">Role for {item.displayName}</span>
                        <select value={item.role} onchange={(event) => changeMediaRole(item.handle, event.currentTarget.value as MediaRole)}>
                          {#if !supportedRoles.some((option) => option.value === item.role)}
                            <option value={item.role}>Unsupported · {mediaRoleLabel(item.role)}</option>
                          {/if}
                          {#each supportedRoles as option (option.value)}
                            <option value={option.value}>{option.label}</option>
                          {/each}
                        </select>
                      </label>
                    {:else}
                      <span class="role-chip">{mediaRoleLabel(item.role)}</span>
                    {/if}
                    <div class="media-item__actions">
                      <button class="icon-button" disabled={index === 0} aria-label={`Move ${item.displayName} earlier`} onclick={() => moveMedia(index, -1)}>↑</button>
                      <button class="icon-button" disabled={index === snapshot.draft.media.length - 1} aria-label={`Move ${item.displayName} later`} onclick={() => moveMedia(index, 1)}>↓</button>
                      <button class="icon-button icon-button--danger" aria-label={`Remove ${item.displayName}`} onclick={() => removeMedia(item.handle)}><Icon name="trash" size={16} /></button>
                    </div>
                  </li>
                {/each}
              </ol>
            {/if}
          </section>
        </div>

        <aside class="compose-inspector" aria-label="Provider and generation settings">
          <section class="card inspector-card">
            <div class="inspector-title"><span class="step-label">03 / THE CAMERA</span><span class="live-catalog">Model catalog</span></div>
            <label class="field">
              <span>Provider</span>
              <select value={snapshot.draft.providerId} onchange={(event) => changeProvider(event.currentTarget.value as ProviderId)}>
                {#each snapshot.providers as provider (provider.id)}
                  <option value={provider.id}>{provider.name}{provider.connected ? ' · Connected' : ' · Needs key'}</option>
                {/each}
              </select>
            </label>

            <label class="field model-field">
              <span>Model</span>
              <select title={selectedModel?.name} value={snapshot.draft.modelId} onchange={(event) => changeModel(event.currentTarget.value)}>
                {#if providerModels.length === 0}<option value="">{ready ? 'No models available' : 'Loading models…'}</option>{/if}
                {#each providerModels as model (model.id)}<option value={model.id}>{model.name}</option>{/each}
              </select>
            </label>

            {#if selectedModel}
              <div class="model-note">
                <p>{selectedModel.description}</p>
                <div class="capabilities" aria-label="Model capabilities">
                  {#if modelSupportsImages(selectedModel)}<span>Image</span>{/if}
                  {#if selectedModel.capabilities.video}<span>Video</span>{/if}
                  {#if selectedModel.capabilities.audioReferences}<span>Audio ref</span>{/if}
                  {#if selectedModel.capabilities.generatedAudio}<span>Soundtrack</span>{/if}
                </div>
              </div>
            {/if}

            <div class="settings-grid">
              <label class="field"><span>Duration</span><select value={snapshot.draft.settings.duration} onchange={(event) => editDraft((draft) => (draft.settings.duration = event.currentTarget.value))}><option value="">Provider default</option>{#if snapshot.draft.settings.duration && !selectedModel?.durationOptions.includes(snapshot.draft.settings.duration)}<option value={snapshot.draft.settings.duration}>Unsupported · {snapshot.draft.settings.duration}</option>{/if}{#each selectedModel?.durationOptions ?? [] as option}<option value={option}>{option}</option>{/each}</select></label>
              <label class="field"><span>Resolution</span><select value={snapshot.draft.settings.resolution} onchange={(event) => changeResolution(event.currentTarget.value)}><option value="">Provider default</option>{#if snapshot.draft.settings.resolution && !selectedModel?.resolutionOptions.includes(snapshot.draft.settings.resolution)}<option value={snapshot.draft.settings.resolution}>Unsupported · {snapshot.draft.settings.resolution}</option>{/if}{#each selectedModel?.resolutionOptions ?? [] as option}<option value={option}>{option}</option>{/each}</select></label>
              <label class="field"><span>Aspect ratio</span><select value={snapshot.draft.settings.aspectRatio} onchange={(event) => changeAspectRatio(event.currentTarget.value)}><option value="">Provider default</option>{#if snapshot.draft.settings.aspectRatio && !selectedModel?.aspectRatioOptions.includes(snapshot.draft.settings.aspectRatio)}<option value={snapshot.draft.settings.aspectRatio}>Unsupported · {snapshot.draft.settings.aspectRatio}</option>{/if}{#each selectedModel?.aspectRatioOptions ?? [] as option}<option value={option}>{option}</option>{/each}</select></label>
              {#if (selectedModel?.sizeOptions?.length ?? 0) > 0 || snapshot.draft.settings.size}
                <label class="field"><span>Output size</span><select value={snapshot.draft.settings.size} onchange={(event) => changeOutputSize(event.currentTarget.value)}><option value="">Provider default</option>{#if snapshot.draft.settings.size && !selectedModel?.sizeOptions?.includes(snapshot.draft.settings.size)}<option value={snapshot.draft.settings.size}>Unsupported · {snapshot.draft.settings.size}</option>{/if}{#each selectedModel?.sizeOptions ?? [] as option}<option value={option}>{option}</option>{/each}</select></label>
              {/if}
              <label class="field"><span>Generated audio</span><select disabled={!selectedModel || (!selectedModel.capabilities.generatedAudio && snapshot.draft.settings.generatedAudio === 'provider_default')} value={snapshot.draft.settings.generatedAudio} onchange={(event) => editDraft((draft) => (draft.settings.generatedAudio = event.currentTarget.value as GenerationDraft['settings']['generatedAudio']))}><option value="provider_default">Provider default</option><option value="on" disabled={!selectedModel?.capabilities.generatedAudio}>On</option><option value="off" disabled={!selectedModel?.capabilities.generatedAudio}>Off</option></select></label>
              <label class="field field--wide"><span>Seed <small>{selectedModel?.capabilities.seed === false ? 'not supported' : 'optional'}</small></span><input inputmode="numeric" maxlength="20" disabled={selectedModel?.capabilities.seed === false && !snapshot.draft.settings.seed} placeholder="Random" value={snapshot.draft.settings.seed} oninput={(event) => editDraft((draft) => (draft.settings.seed = event.currentTarget.value))} onblur={() => void flushLatestDraft().catch(() => undefined)} /></label>
              <details class="advanced-settings field--wide">
                <summary>Extra model settings <small>JSON · optional</small></summary>
                <label class="sr-only" for="advanced-json">Extra model settings JSON</label>
                <textarea id="advanced-json" rows="5" maxlength="100000" spellcheck="false" placeholder={'{\n  "guidance_scale": 5\n}'} value={snapshot.draft.settings.advancedJson} oninput={(event) => editDraft((draft) => (draft.settings.advancedJson = event.currentTarget.value))} onblur={() => void flushLatestDraft().catch(() => undefined)}></textarea>
                <p>Advanced values are sent to the selected model. Review shows the exact saved object before payment.</p>
              </details>
            </div>
          </section>

          <section class="review-launcher" aria-label="Review readiness">
            <div class="readiness"><span class:ready={canReview} class="readiness__dot" aria-hidden="true"></span><p>{compatibilityMessage()}</p></div>
            {#if snapshot.preparedReview}
              <button class="button button--secondary button--full" onclick={() => reviewDialog.showModal()}>Open Review</button>
            {/if}
            <button class="button button--primary button--full" disabled={!canReview} onclick={beginReview}>
              <Icon name="spark" size={17} /> {isPreparing ? 'Preparing Review…' : 'Review this shot'}
            </button>
            <p class="billing-note">Review may upload the local files above. It does not start a paid generation.</p>
          </section>
        </aside>
      </div>
    </main>
  {:else if activeView === 'jobs'}
    <main class="workspace jobs-page">
      <section class="page-heading jobs-heading" bind:this={jobsHeadingElement} tabindex="-1">
        <div><p class="eyebrow">Screening room</p><h1>Your films, taking shape.</h1><p>Watch each render grow, then play the finished cut.</p></div>
        <div class="job-summary"><strong>{activeJobCount}</strong><span>active {activeJobCount === 1 ? 'render' : 'renders'}</span></div>
      </section>

      <div class="jobs-layout">
        <aside class="jobs-sidebar" aria-label="Generation jobs">
          <div class="jobs-tools">
            <label class="search-field"><span class="sr-only">Search renders</span><Icon name="search" size={17} /><input value={jobSearch} oninput={(event) => updateJobSearch(event.currentTarget.value)} type="search" placeholder="Search renders" /></label>
            <label class="sr-only" for="job-filter">Filter jobs</label>
            <select id="job-filter" class="filter-select" value={jobFilter} onchange={(event) => updateJobFilter(event.currentTarget.value as typeof jobFilter)}><option value="all">All</option><option value="active">Active</option><option value="attention">Needs attention</option><option value="completed">Completed</option></select>
          </div>
          {#if filteredJobs.length === 0}
            <div class="sidebar-empty"><Icon name="film" size={25} /><p>{snapshot.jobs.length === 0 ? 'No renders yet. Review a shot to begin.' : 'Nothing matches this reel.'}</p></div>
          {:else}
            <div class="job-list">
              {#each filteredJobs as job (job.id)}
                <button class:selected={snapshot.selectedJobId === job.id} class="job-row" aria-current={snapshot.selectedJobId === job.id ? 'true' : undefined} onclick={() => void selectJob(job.id)}>
                  <span class={`status-orb status-orb--${statusTone(job)}`} aria-hidden="true"></span>
                  <span class="job-row__copy"><strong>{job.prompt}</strong><small>{job.providerName} · {job.modelName}</small><span>{job.statusLabel}</span></span>
                  <time datetime={job.createdAt}>{formatDate(job.createdAt)}</time>
                  <Icon name="chevron" size={16} />
                </button>
              {/each}
            </div>
          {/if}
        </aside>

        <section class="job-detail" aria-live="off">
          {#if selectedJob}
            <div class="detail-header">
              <div><span class={`status-pill status-pill--${statusTone(selectedJob)}`}>{selectedJob.statusLabel}</span><h2>{selectedJob.prompt}</h2><p>{selectedJob.providerName} · {selectedJob.modelName}</p></div>
              <div class="detail-actions">
                {#if canPauseMonitoring(selectedJob) || canResumeMonitoring(selectedJob)}<button class="button button--secondary" disabled={Boolean(monitorBusy[selectedJob.id])} onclick={() => void toggleJobMonitoring()}><Icon name={canResumeMonitoring(selectedJob) ? 'play' : 'pause'} size={16} /> {monitorBusy[selectedJob.id] ? 'Updating…' : canResumeMonitoring(selectedJob) ? 'Resume updates' : 'Pause updates'}</button>{/if}
                {#if selectedJob.status === 'completed' && selectedJob.hasLocalOutput}
                  <button class="button button--primary" disabled={outputBusy || playbackBusy} onclick={() => void playSelectedOutput()}><Icon name="play" size={16} /> {playbackBusy ? 'Loading…' : 'Play here'}</button>
                  <button class="button button--secondary" disabled={outputBusy || playbackBusy} onclick={() => void openSelectedOutput()}><Icon name="external" size={16} /> {outputBusy ? 'Opening…' : 'Open in player'}</button>
                {/if}
                {#if selectedJob.deletable}<button class="icon-button icon-button--danger" disabled={playbackBusy || deleteBusy} aria-label="Delete render" title="Delete render" onclick={askToDeleteRender}><Icon name="trash" size={17} /></button>{/if}
              </div>
            </div>

            {#if selectedJob.status === 'completed' && selectedJob.hasLocalOutput}
              <section class="player-card" aria-labelledby="player-heading">
                <div class="player-card__heading"><div><p class="micro-label">Finished film</p><h3 id="player-heading">{selectedJob.outputFileName ?? 'Generated video'}</h3></div><button class="text-button" disabled={playbackBusy} onclick={() => void playSelectedOutput()}>{playbackBusy ? 'Loading…' : playbackUrl ? 'Play again' : 'Load & play'}</button></div>
                {#if selectedJob.captionsUrl}
                  <video bind:this={videoElement} controls preload="metadata" poster="/demo-poster.svg" src={playbackUrl || selectedJob.playbackUrl || undefined} aria-label={`Generated video: ${selectedJob.prompt}`}>
                    <track kind="captions" srclang="en" label="English" src={selectedJob.captionsUrl} default />
                  </video>
                {:else}
                  <!-- svelte-ignore a11y_media_has_caption -- provider output has no caption resource; absence is stated immediately below -->
                  <video bind:this={videoElement} controls preload="metadata" poster="/demo-poster.svg" src={playbackUrl || selectedJob.playbackUrl || undefined} aria-label={`Generated video: ${selectedJob.prompt}`}></video>
                {/if}
                <div class="player-meta"><span><Icon name="check" size={15} /> Saved in your Videos folder</span>{#if !selectedJob.captionsUrl}<span>This model didn’t include captions.</span>{/if}</div>
              </section>
            {:else}
              <CloudCinema active={isActivelyMonitored(selectedJob)} paused={selectedJob.monitorState === 'paused' || selectedJob.status === 'paused'} provider={selectedJob.providerName} status={selectedJob.statusLabel} detail={selectedJob.detail} jobId={selectedJob.id} elapsedSeconds={selectedJob.elapsedSeconds} nextPollSeconds={selectedJob.nextPollSeconds} />
            {/if}

            <div class="detail-grid">
              <section class="card detail-card"><p class="micro-label">Latest update</p><h3>{selectedJob.detail}</h3>{#if selectedJob.remoteContinues}<p class="warning-copy">Local updates are paused. The provider may still be working and charging.</p>{/if}</section>
              <section class="card detail-card provider-id-card">
                <div class="provider-id-heading"><div><p class="micro-label">{selectedJob.providerJobId ? 'Provider job ID' : 'Local job handle'}</p><span>{selectedJob.providerName}</span></div><button class="text-button copy-id-button" aria-label={selectedJob.providerJobId ? 'Copy provider job ID' : 'Copy local job handle'} onclick={() => void copyJobIdentifier(selectedJobIdentifier, selectedJob.id)}>{copiedJobId === selectedJob.id ? 'Copied ✓' : 'Copy ID'}</button></div>
                <code bind:this={jobIdentifierElement} class="provider-id-value" tabindex="-1">{selectedJobIdentifier}</code>
                <p>{selectedJob.providerJobId ? 'Handy if you need to find this render with the provider.' : 'This local handle identifies the render inside Video Harness.'}</p>
                <p>Started {formatDate(selectedJob.createdAt)}</p>
              </section>
            </div>
          {:else}
            <div class="detail-empty"><div class="empty-illustration"><Icon name="film" size={30} /></div><h2>Pick a render</h2><p>Its progress, Tiny Cloud Cinema, and finished video will appear here.</p></div>
          {/if}
        </section>
      </div>
    </main>
  {:else}
    <main class="workspace providers-page">
      <section class="page-heading"><div><p class="eyebrow">Backstage</p><h1>Providers &amp; keys</h1><p>Connect the services that make the magic. Keys stay out of drafts and history.</p></div><div class="privacy-chip"><Icon name="key" size={16} /> System keyring when available</div></section>
      <div class="provider-grid">
        {#each snapshot.providers as provider (provider.id)}
          <section class:connected={provider.connected} class="card provider-card">
            <div class="provider-card__header">
              <div class={`provider-monogram provider-monogram--${provider.id}`}>{provider.shortName}</div>
              <div><p class="micro-label">{provider.connected ? 'Connected' : 'Not connected'}</p><h2>{provider.name}</h2></div>
              <span class:online={provider.connected} class="connection-dot" aria-hidden="true"></span>
            </div>
            <p class="provider-description">{provider.description}</p>
            <div class="provider-note"><Icon name="paperclip" size={17} /><p>{provider.localMediaNote}</p></div>
            {#if provider.connected}
              <div class="credential-state"><div><strong>{provider.accountLabel ?? 'Key connected'}</strong><span>{provider.credentialStorage === 'keyring' ? 'Safe in your system keyring' : 'Here for this session only'}</span></div><Icon name="check" size={20} /></div>
            {/if}
            <form class="credential-form" onsubmit={(event) => { event.preventDefault(); void connectProvider(provider.id); }}>
              <label class="field field--wide"><span>{provider.connected ? 'Replace API key' : 'API key'}</span><input bind:value={providerKeys[provider.id]} type="password" autocomplete="off" autocapitalize="off" spellcheck="false" placeholder={provider.id === 'openrouter' ? 'sk-or-v1-…' : 'fal_key_…'} /></label>
              <label class="check-row"><input bind:checked={providerRemember[provider.id]} type="checkbox" /><span><strong>Remember this key</strong><small>Uses your system keyring when available; otherwise this session only.</small></span></label>
              <div class="provider-actions"><button class="button button--primary" disabled={providerBusy[provider.id]} type="submit">{providerBusy[provider.id] ? 'Checking…' : provider.connected ? 'Replace key' : `Connect ${provider.name}`}</button>{#if provider.connected}<button class="button button--danger" disabled={providerBusy[provider.id]} type="button" onclick={() => void forgetProvider(provider.id)}>Forget key</button>{/if}</div>
            </form>
          </section>
        {/each}
      </div>
      {#if snapshot.safetyHolds.length > 0}
        <section class="safety-holds" aria-labelledby="safety-holds-title">
          <div class="safety-holds__heading"><Icon name="warning" size={22} /><div><p class="eyebrow">Double-charge guard</p><h2 id="safety-holds-title">These requests need a quick check.</h2><p>Each hold stops the exact request from being submitted—and possibly billed—twice.</p></div></div>
          <div class="safety-holds__list">
            {#each snapshot.safetyHolds as hold (hold.handle)}
              <article class="card safety-hold">
                <div><p class="micro-label">{hold.providerName} · {formatDate(hold.recordedAt)}</p><h3>We don’t know whether the provider accepted this request.</h3><p>{hold.message}</p></div>
                <label class="check-row"><input type="checkbox" bind:checked={confirmedSafetyHolds[hold.handle]} /><span><strong>I checked the {hold.providerName} dashboard</strong><small>I understand that clearing this hold permits this exact paid request again.</small></span></label>
                <button class="button button--danger" type="button" disabled={!confirmedSafetyHolds[hold.handle] || holdBusy[hold.handle]} onclick={() => void acknowledgeSafetyHold(hold.handle)}>{holdBusy[hold.handle] ? 'Clearing hold…' : 'Dashboard checked — clear hold'}</button>
              </article>
            {/each}
          </div>
        </section>
      {/if}
      <section class="privacy-note"><Icon name="key" size={22} /><div><h2>Your key stays backstage.</h2><p>It goes straight to the Rust credential service, then leaves the form. No browser storage, logs, drafts, or job events.</p></div></section>
    </main>
  {/if}
</div>

<dialog class="sheet-dialog delete-dialog" bind:this={deleteDialog} aria-labelledby="delete-title" oncancel={(event) => deleteBusy && event.preventDefault()}>
  <div class="dialog-accent dialog-accent--danger" aria-hidden="true"></div>
  <div class="dialog-heading"><div class="dialog-icon dialog-icon--danger"><Icon name="trash" size={21} /></div><div><p class="eyebrow">Clear the reel</p><h2 id="delete-title">Remove this render?</h2></div><button class="icon-button" aria-label="Cancel render deletion" disabled={deleteBusy} onclick={() => deleteDialog.close()}><Icon name="x" size={18} /></button></div>
  <div class="dialog-body delete-dialog__body">
    <p>It’ll disappear from Video Harness. {selectedJob?.providerName ?? 'The provider'} keeps its own copy and job record.</p>
    {#if selectedJob?.hasLocalOutput}<div class="delete-file-note"><Icon name="video" size={18} /><span><strong>{selectedJob.outputFileName ?? 'Saved generated video'}</strong><small>You choose whether this saved file stays in your Videos folder.</small></span></div>{/if}
    {#if deleteError}<div class="dialog-error" role="alert"><Icon name="warning" size={17} /><p>{deleteError}</p></div>{/if}
  </div>
  <div class="dialog-actions delete-dialog__actions">
    <button class="button button--secondary" disabled={deleteBusy} onclick={() => deleteDialog.close()}>Never mind</button>
    <button class="button button--secondary" disabled={deleteBusy} onclick={() => void deleteSelectedRender(false)}>{deleteBusy ? 'Clearing…' : selectedJob?.hasLocalOutput ? 'Remove, keep video' : 'Remove from reel'}</button>
    {#if selectedJob?.hasLocalOutput}<button class="button button--danger" disabled={deleteBusy} onclick={() => void deleteSelectedRender(true)}><Icon name="trash" size={16} /> {deleteBusy ? 'Deleting…' : 'Delete video too'}</button>{/if}
  </div>
</dialog>

<dialog class="sheet-dialog" bind:this={uploadDialog} aria-labelledby="upload-title">
  <div class="dialog-accent dialog-accent--warning" aria-hidden="true"></div>
  <div class="dialog-heading"><div class="dialog-icon dialog-icon--warning"><Icon name="paperclip" size={22} /></div><div><p class="eyebrow">Before Review</p><h2 id="upload-title">One small detour: these files need a public link.</h2></div><button class="icon-button" aria-label="Cancel upload" onclick={() => uploadDialog.close()}><Icon name="x" size={18} /></button></div>
  <div class="dialog-body">
    <p>OpenRouter accepts reference media by public HTTPS URL. To prepare this Review, Video Harness will upload <strong>{snapshot.draft.media.filter((item) => item.source === 'local').length} local {snapshot.draft.media.filter((item) => item.source === 'local').length === 1 ? 'file' : 'files'}</strong> to fal.ai's public-by-link CDN.</p>
    <ul class="disclosure-list"><li>Anyone with the link can download the file.</li><li>Video Harness asks for the link to expire after 24 hours.</li><li>The link is shared with OpenRouter and the selected model provider.</li><li>This only prepares Review; starting the paid generation takes a separate click.</li></ul>
    <div class="safety-callout"><Icon name="check" size={18} /><span>Choose Keep files local and nothing leaves this device.</span></div>
  </div>
  <div class="dialog-actions"><button class="button button--secondary" onclick={() => uploadDialog.close()}>Keep files local</button><button class="button button--primary" onclick={() => void prepareReview(true)}>Upload for Review</button></div>
</dialog>

<dialog class="sheet-dialog review-dialog" bind:this={reviewDialog} aria-labelledby="review-title" aria-describedby={isSubmitting ? 'submission-state' : undefined} oncancel={(event) => { event.preventDefault(); closeReview(); }}>
  {#if snapshot.preparedReview}
    {@const review = snapshot.preparedReview}
    <div class="dialog-accent" aria-hidden="true"></div>
    <div class="dialog-heading"><div class="dialog-icon"><Icon name="spark" size={22} /></div><div><p class="eyebrow">Final check</p><h2 id="review-title">One last look before the lights go down.</h2></div><button class="icon-button" aria-label="Close Review" disabled={isSubmitting} onclick={closeReview}><Icon name="x" size={18} /></button></div>
    <div class="review-price"><div><span>Fresh estimate</span><strong>{review.estimatedCost}</strong></div><p>Estimate only — your provider’s final usage is what counts.</p></div>
    <div class="dialog-body review-body">
      {#if review.uploadDisclosure}<div class="review-disclosure"><Icon name="warning" size={18} /><p>{review.uploadDisclosure}</p></div>{/if}
      <section><p class="micro-label">Prompt</p><p class="review-prompt">{review.prompt}</p></section>
      <div class="review-facts"><div><span>Provider</span><strong>{review.providerName}</strong></div><div><span>Model</span><strong title={review.modelName}>{review.modelName}</strong></div><div><span>Duration</span><strong>{review.settings.duration || 'Provider default'}</strong></div><div><span>Output</span><strong>{review.settings.resolution || 'Provider default'} · {review.settings.aspectRatio || 'Provider default'}</strong></div>{#if review.settings.size}<div><span>Exact size</span><strong>{review.settings.size}</strong></div>{/if}<div><span>Generated audio</span><strong>{review.settings.generatedAudio === 'on' ? 'On' : review.settings.generatedAudio === 'off' ? 'Off' : 'Provider default'}</strong></div><div><span>Seed</span><strong>{review.settings.seed || 'Random / provider default'}</strong></div></div>
      {#if review.advancedSettingsJson}<section class="review-advanced"><div><p class="micro-label">Extra model settings</p><p>These saved settings are included in this paid request.</p></div><pre>{review.advancedSettingsJson}</pre></section>{/if}
      {#if review.media.length > 0}<section><p class="micro-label">Reference media</p><ul class="review-media">{#each review.media as item (item.handle)}<li><Icon name={item.kind} size={16} /><span>{item.displayName}{#if item.displayUrl}<small title={item.displayUrl}>{item.displayUrl}</small>{/if}</span><small>{mediaRoleLabel(item.role)}</small></li>{/each}</ul></section>{/if}
      <p class="review-expiry"><Icon name="clock" size={15} /> Review expires at {formatDate(review.expiresAt)}. Any edit makes a fresh Review.</p>
      {#if isSubmitting}<div id="submission-state" class="submission-state" role="status"><Icon name="clock" size={18} /><p><strong>Submitting exactly once…</strong><span>This paid request is in flight. Review stays locked until the provider outcome is known.</span>{#if closeRequestedAfterSubmission}<span>Video Harness will save and close after that outcome arrives.</span>{/if}</p></div>{/if}
      {#if reviewError}<div class="dialog-error" role="alert"><Icon name="warning" size={17} /><p>{reviewError}</p></div>{/if}
    </div>
    <div class="dialog-actions dialog-actions--paid"><button class="button button--secondary" disabled={isSubmitting} onclick={closeReview}>Go back</button><div><span>Exactly one paid provider request</span><button class="button button--paid" disabled={isSubmitting} onclick={() => void submitReview()}>{isSubmitting ? 'Submitting once…' : 'Generate — one paid request'} <Icon name="spark" size={16} /></button></div></div>
  {/if}
</dialog>

<dialog class="sheet-dialog close-dialog" bind:this={closeDialog} aria-labelledby="close-title" aria-describedby="close-description" oncancel={(event) => { event.preventDefault(); void cancelCloseRequest(); }}>
  <div class="dialog-accent" aria-hidden="true"></div>
  <div class="dialog-heading"><div class="dialog-icon"><Icon name="check" size={21} /></div><div><p class="eyebrow">Before closing</p><h2 id="close-title">Save this scene safely?</h2></div></div>
  <div class="dialog-body">
    <p id="close-description">Video Harness is saving the latest draft on this device before the window closes.</p>
    {#if closeBusy}<div class="submission-state" role="status"><Icon name="clock" size={18} /><p><strong>{closeCommitted ? 'Draft saved. Finishing background work…' : 'Saving the latest edit…'}</strong><span>{closeCommitted ? 'The close request is committed. Video Harness will exit as soon as in-flight work reaches a safe stopping point.' : 'Keep this window open until the save is confirmed.'}</span>{#if closeWaitMessage}<span>{closeWaitMessage}</span>{/if}</p></div>{/if}
    {#if closeError}<div class="dialog-error" role="alert"><Icon name="warning" size={17} /><p><strong>{closeErrorTitle || 'Video Harness stayed open.'}</strong><span>{closeError}</span></p></div>{/if}
  </div>
  <div class="dialog-actions"><button class="button button--secondary" disabled={closeBusy || closeCancelBusy || closeCommitted} onclick={() => void cancelCloseRequest()}>{closeCancelBusy ? 'Cancelling…' : closeCancelFailed ? 'Retry Keep working' : 'Keep working'}</button><button class="button button--primary" disabled={closeBusy || closeCancelBusy || closeCommitted} onclick={() => void beginCloseSave()}>{closeCommitted ? 'Closing safely…' : closeBusy ? 'Saving…' : closeError ? 'Retry save & close' : 'Save & close'}</button></div>
</dialog>

{#if notice}
  <div class={`toast toast--${notice.tone}`} role={notice.tone === 'danger' ? 'alert' : 'status'}><span aria-hidden="true">{notice.tone === 'good' ? '✓' : notice.tone === 'danger' ? '!' : '◆'}</span><p>{notice.message}</p><button aria-label="Dismiss notification" onclick={() => (notice = null)}><Icon name="x" size={15} /></button></div>
{/if}

<div class="sr-only" aria-live="polite" aria-atomic="true">{liveMessage}</div>
