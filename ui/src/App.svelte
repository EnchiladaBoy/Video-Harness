<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { createBridge } from './lib/bridge';
  import { physicalPointToCss, pointInRect } from './lib/geometry';
  import { demoSnapshot } from './lib/mock-bridge';
  import { applySequencedEvent, reconcileSnapshot } from './lib/state';
  import CloudCinema from './lib/components/CloudCinema.svelte';
  import Icon from './lib/components/Icon.svelte';
  import type {
    AppSnapshot,
    FileDropEvent,
    GenerationDraft,
    JobStatus,
    MediaItem,
    MediaKind,
    MediaRole,
    ProviderId,
    UiEventEnvelope,
    WorkspaceView
  } from './lib/types';
  import { isActiveJob } from './lib/types';

  const bridge = createBridge();

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
          generatedAudio: 'provider_default',
          seed: ''
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
  let providerBusy = $state<ProviderId | null>(null);
  let holdBusy = $state<string | null>(null);
  let confirmedSafetyHolds = $state<Record<string, boolean>>({});
  let playbackUrl = $state('');
  let playbackGrantId = '';
  let playbackBusy = $state(false);
  let playbackEpoch = 0;
  let playbackQueue: Promise<void> = Promise.resolve();
  let videoElement = $state<HTMLVideoElement>();
  let jobIdentifierElement = $state<HTMLElement>();
  let outputBusy = $state(false);
  let deleteBusy = $state(false);
  let copiedJobId = $state('');
  let reviewDialog: HTMLDialogElement;
  let uploadDialog: HTMLDialogElement;
  let deleteDialog: HTMLDialogElement;
  let dropZone = $state<HTMLDivElement>();
  let saveTimer: number | undefined;
  let noticeTimer: number | undefined;
  let copyTimer: number | undefined;

  let providerModels = $derived(
    snapshot.models.filter((model) => model.providerId === snapshot.draft.providerId)
  );
  let selectedModel = $derived(
    providerModels.find((model) => model.id === snapshot.draft.modelId)
  );
  let selectedProvider = $derived(
    snapshot.providers.find((provider) => provider.id === snapshot.draft.providerId)
  );
  let falProvider = $derived(snapshot.providers.find((provider) => provider.id === 'fal'));
  let selectedJob = $derived(snapshot.jobs.find((job) => job.id === snapshot.selectedJobId));
  let selectedJobIdentifier = $derived(selectedJob?.providerJobId ?? selectedJob?.id ?? '');
  let activeJobCount = $derived(snapshot.jobs.filter((job) => isActiveJob(job.status)).length);
  let requiresOpenRouterUpload = $derived(
    snapshot.draft.providerId === 'openrouter' &&
      snapshot.draft.media.some((item) => item.source === 'local')
  );
  let hasCompatibleMedia = $derived(
    !snapshot.draft.media.some(
      (item) =>
        (item.kind === 'image' && !selectedModel?.capabilities.images) ||
        (item.kind === 'video' && !selectedModel?.capabilities.video) ||
        (item.kind === 'audio' && !selectedModel?.capabilities.audioReferences)
    )
  );
  let canReview = $derived(
    ready &&
      !isPreparing &&
      snapshot.draft.prompt.trim().length > 0 &&
      Boolean(selectedModel) &&
      Boolean(selectedProvider?.connected) &&
      (!requiresOpenRouterUpload || Boolean(falProvider?.connected)) &&
      hasCompatibleMedia
  );
  let filteredJobs = $derived.by(() => {
    const query = jobSearch.trim().toLowerCase();
    return snapshot.jobs.filter((job) => {
      const matchesText =
        !query ||
        job.prompt.toLowerCase().includes(query) ||
        job.modelName.toLowerCase().includes(query) ||
        job.id.toLowerCase().includes(query);
      const matchesFilter =
        jobFilter === 'all' ||
        (jobFilter === 'active' && isActiveJob(job.status)) ||
        (jobFilter === 'attention' && (job.status === 'attention' || job.status === 'paused')) ||
        (jobFilter === 'completed' && job.status === 'completed');
      return matchesText && matchesFilter;
    });
  });

  function announce(message: string): void {
    liveMessage = '';
    window.setTimeout(() => (liveMessage = message), 20);
  }

  function showNotice(
    message: string,
    tone: 'neutral' | 'good' | 'warning' | 'danger' = 'neutral'
  ): void {
    notice = { message, tone };
    announce(message);
    if (noticeTimer) window.clearTimeout(noticeTimer);
    noticeTimer = window.setTimeout(() => (notice = null), tone === 'danger' ? 8_000 : 4_500);
  }

  function replayBufferedEvents(): void {
    const replay = bufferedEvents.sort((left, right) => left.seq - right.seq);
    bufferedEvents = [];
    for (const envelope of replay) {
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
        snapshot = reconcileSnapshot(snapshot, current.snapshot);
        sequence = current.seq;
      } catch (error) {
        showNotice(errorMessage(error), 'danger');
      } finally {
        acceptingEvents = true;
        replayBufferedEvents();
        resyncInFlight = undefined;
      }
    })();
    return resyncInFlight;
  }

  function consumeEvent(envelope: UiEventEnvelope): void {
    const result = applySequencedEvent(snapshot, sequence, envelope);
    snapshot = result.snapshot;
    sequence = result.seq;
    if (result.gap) void resyncSnapshot();

    const event = envelope.event;
    if (event.type === 'snapshot_changed') {
      if (!event.snapshot.preparedReview) reviewDialog?.close();
    } else if (event.type === 'review_ready') {
      isPreparing = false;
      announce('Review ready. Nothing paid happens until you press Generate.');
      window.setTimeout(() => reviewDialog?.showModal(), 0);
    } else if (event.type === 'job_added') {
      isSubmitting = false;
      reviewDialog?.close();
      activeView = 'jobs';
      announce('The provider accepted your request. Tiny Cloud Cinema is keeping watch.');
    } else if (event.type === 'review_invalidated') {
      isPreparing = false;
      reviewDialog?.close();
    } else if (event.type === 'operation_failed') {
      if (event.operation === 'preparation') isPreparing = false;
      else {
        isSubmitting = false;
        reviewDialog?.close();
      }
      showNotice(event.message, 'danger');
    } else if (event.type === 'job_updated' && event.job.status === 'completed') {
      announce('Your video is ready to watch.');
    } else if (event.type === 'job_removed') {
      deleteDialog?.close();
    } else if (event.type === 'notice') {
      showNotice(event.message, event.tone);
    }
  }

  function receiveEvent(envelope: UiEventEnvelope): void {
    if (!acceptingEvents) bufferedEvents.push(envelope);
    else consumeEvent(envelope);
  }

  onMount(() => {
    let disposed = false;
    let dropSubscription: { close(): void } | undefined;

    void bridge
      .openSession(receiveEvent)
      .then((session) => {
        if (disposed) return;
        snapshot = reconcileSnapshot(snapshot, session.snapshot);
        sequence = session.seq;
        acceptingEvents = true;
        replayBufferedEvents();
        ready = true;
      })
      .catch((error) => {
        sessionFailed = errorMessage(error);
        ready = false;
      });

    void bridge.watchFileDrops(handleNativeDrop).then((subscription) => {
      if (disposed) subscription.close();
      else dropSubscription = subscription;
    });

    return () => {
      disposed = true;
      dropSubscription?.close();
      if (saveTimer) window.clearTimeout(saveTimer);
      if (noticeTimer) window.clearTimeout(noticeTimer);
      if (copyTimer) window.clearTimeout(copyTimer);
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

  function scheduleDraftSave(draft: GenerationDraft): void {
    if (saveTimer) window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      void bridge.saveDraft(draft).catch((error) => showNotice(errorMessage(error), 'danger'));
    }, 650);
  }

  function draftSnapshot(): GenerationDraft {
    // Svelte 5 deep state is a Proxy and cannot be passed to
    // structuredClone. Snapshotting produces the plain, serializable draft
    // expected by the Rust bridge and local immutable edits.
    return $state.snapshot(snapshot.draft);
  }

  function editDraft(change: (draft: GenerationDraft) => void): void {
    const hadReview = Boolean(snapshot.preparedReview);
    const hadActivePreparation = isPreparing;
    const draft = draftSnapshot();
    change(draft);
    draft.revision += 1;
    snapshot = { ...snapshot, draft, preparedReview: undefined, draftSaved: false };
    if (hadReview || hadActivePreparation) {
      isPreparing = false;
      reviewDialog?.close();
      void bridge.invalidatePrepared(draft.revision);
    }
    scheduleDraftSave(draft);
  }

  function changeProvider(providerId: ProviderId): void {
    const firstModel = snapshot.models.find((model) => model.providerId === providerId);
    editDraft((draft) => {
      draft.providerId = providerId;
      draft.modelId = firstModel?.id ?? '';
      if (firstModel) {
        draft.settings.duration = firstModel.durationOptions[0] ?? '';
        draft.settings.resolution = firstModel.resolutionOptions[0] ?? '';
        draft.settings.aspectRatio = firstModel.aspectRatioOptions[0] ?? '';
      }
    });
  }

  function changeModel(modelId: string): void {
    const model = providerModels.find((item) => item.id === modelId);
    editDraft((draft) => {
      draft.modelId = modelId;
      if (model) {
        draft.settings.duration = model.durationOptions[0] ?? '';
        draft.settings.resolution = model.resolutionOptions[0] ?? '';
        draft.settings.aspectRatio = model.aspectRatioOptions[0] ?? '';
        if (!model.capabilities.generatedAudio) draft.settings.generatedAudio = 'provider_default';
      }
    });
  }

  function appendMedia(items: MediaItem[]): void {
    if (items.length === 0) return;
    editDraft((draft) => draft.media.push(...items));
    showNotice(`${items.length} ${items.length === 1 ? 'reference' : 'references'} tucked in.`, 'good');
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
    try {
      const role =
        remoteKind === 'video'
          ? 'video_reference'
          : remoteKind === 'audio'
            ? 'audio_reference'
            : remoteRole;
      const item = await bridge.addRemoteMedia(remoteUrl.trim(), remoteKind, role);
      appendMedia([item]);
      remoteUrl = '';
      showRemoteForm = false;
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
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
      await bridge.saveDraft(reviewedDraft);
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
    try {
      await bridge.submitPrepared(review.preparedId);
    } catch (error) {
      isSubmitting = false;
      showNotice(errorMessage(error), 'danger');
    }
  }

  async function connectProvider(providerId: ProviderId): Promise<void> {
    const key = providerKeys[providerId].trim();
    if (!key) {
      showNotice('Paste a key first.', 'warning');
      return;
    }
    providerKeys[providerId] = '';
    providerBusy = providerId;
    try {
      await bridge.connectProvider(providerId, key, providerRemember[providerId]);
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      providerBusy = null;
    }
  }

  async function forgetProvider(providerId: ProviderId): Promise<void> {
    providerBusy = providerId;
    try {
      await bridge.forgetProvider(providerId);
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      providerBusy = null;
    }
  }

  async function acknowledgeSafetyHold(handle: string): Promise<void> {
    if (!confirmedSafetyHolds[handle]) return;
    holdBusy = handle;
    try {
      await bridge.acknowledgeSafetyHold(handle);
      showNotice('Dashboard check sent. You can Review this exact request once the hold clears.', 'good');
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
    } finally {
      holdBusy = null;
    }
  }

  async function selectJob(jobId: string): Promise<void> {
    if (snapshot.selectedJobId !== jobId) {
      playbackEpoch += 1;
      await releasePlayback();
    }
    snapshot = { ...snapshot, selectedJobId: jobId };
  }

  async function toggleJobMonitoring(): Promise<void> {
    if (!selectedJob) return;
    try {
      if (selectedJob.status === 'paused') await bridge.resumeJob(selectedJob.id);
      else await bridge.pauseJob(selectedJob.id);
    } catch (error) {
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
    for (let attempt = 0; attempt < 5; attempt += 1) {
      try {
        await bridge.releasePlayback(grantId);
        return true;
      } catch (error) {
        if (attempt === 4) {
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
    if (!job || job.status !== 'completed' || playbackBusy) return false;
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
    if (!selectedJob || selectedJob.status !== 'completed' || outputBusy) return;
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
    deleteDialog.showModal();
  }

  async function deleteSelectedRender(deleteOutput: boolean): Promise<void> {
    const job = selectedJob;
    if (!job?.deletable || deleteBusy) return;
    deleteBusy = true;
    try {
      playbackEpoch += 1;
      if (!(await releasePlayback())) return;
      await bridge.deleteRender(job.id, deleteOutput);
      deleteDialog.close();
      announce('Render removed from your reel.');
      showNotice(
        deleteOutput ? 'Render and saved video deleted.' : 'Render cleared from your reel.',
        'good'
      );
    } catch (error) {
      showNotice(errorMessage(error), 'danger');
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

  function statusTone(status: JobStatus): string {
    if (status === 'completed') return 'good';
    if (status === 'attention') return 'danger';
    if (status === 'paused') return 'warning';
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
    if (!selectedProvider?.connected) return `Connect ${selectedProvider?.name ?? 'a provider'} backstage.`;
    if (!snapshot.draft.prompt.trim()) return 'Add your idea above.';
    if (!selectedModel) return 'Pick a model.';
    const unsupported = snapshot.draft.media.filter(
      (item) =>
        (item.kind === 'image' && !selectedModel?.capabilities.images) ||
        (item.kind === 'video' && !selectedModel?.capabilities.video) ||
        (item.kind === 'audio' && !selectedModel?.capabilities.audioReferences)
    );
    if (unsupported.length > 0) {
      return unsupported.length === 1
        ? 'This model can’t use this reference.'
        : `This model can’t use these ${unsupported.length} references.`;
    }
    if (requiresOpenRouterUpload && !falProvider?.connected) {
      return 'Connect fal.ai backstage to carry these files to OpenRouter.';
    }
    return 'All set for Review.';
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
              <button class="text-button" aria-expanded={showRemoteForm} onclick={() => (showRemoteForm = !showRemoteForm)}>
                <span aria-hidden="true">＋</span> Or add a public link
              </button>
            </div>

            {#if showRemoteForm}
              <form class="remote-form" onsubmit={(event) => { event.preventDefault(); void addRemoteReference(); }}>
                <label class="field field--wide">
                  <span>Public HTTPS URL</span>
                  <input bind:value={remoteUrl} required type="url" pattern="https://.*" placeholder="https://example.com/reference.mp4" />
                </label>
                <label class="field">
                  <span>Media type</span>
                  <select bind:value={remoteKind} onchange={() => {
                    remoteRole = remoteKind === 'video' ? 'video_reference' : remoteKind === 'audio' ? 'audio_reference' : 'reference';
                  }}>
                    <option value="image">Image</option>
                    <option value="video">Video</option>
                    <option value="audio">Audio</option>
                  </select>
                </label>
                {#if remoteKind === 'image'}
                  <label class="field">
                    <span>Role</span>
                    <select bind:value={remoteRole}>
                      <option value="reference">Reference</option>
                      <option value="start_frame">Start frame</option>
                      <option value="end_frame">End frame</option>
                    </select>
                  </label>
                {:else}
                  <div class="fixed-role"><span>Role</span><strong>{remoteKind === 'video' ? 'Video reference' : 'Audio reference'}</strong></div>
                {/if}
                <button class="button button--secondary" type="submit">Add URL</button>
              </form>
            {/if}

            {#if snapshot.draft.media.length > 0}
              <ol class="media-list" aria-label="Ordered reference media">
                {#each snapshot.draft.media as item, index (item.handle)}
                  <li class="media-item">
                    <div class={`media-thumb media-thumb--${item.kind}`}>
                      {#if item.previewUrl}<img src={item.previewUrl} alt="" />{:else}<Icon name={item.kind} size={21} />{/if}
                    </div>
                    <div class="media-item__name">
                      <strong title={item.displayName}>{item.displayName}</strong>
                      <span>{item.detail} · {item.source === 'local' ? 'Local' : 'HTTPS'}</span>
                    </div>
                    {#if item.kind === 'image'}
                      <label class="compact-field">
                        <span class="sr-only">Role for {item.displayName}</span>
                        <select value={item.role} onchange={(event) => changeMediaRole(item.handle, event.currentTarget.value as MediaRole)}>
                          <option value="reference">Reference</option>
                          <option value="start_frame">Start frame</option>
                          <option value="end_frame">End frame</option>
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
            <div class="inspector-title"><span class="step-label">03 / THE CAMERA</span><span class="live-catalog">Live models</span></div>
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
                {#if providerModels.length === 0}<option value="">Loading models…</option>{/if}
                {#each providerModels as model (model.id)}<option value={model.id}>{model.name}</option>{/each}
              </select>
            </label>

            {#if selectedModel}
              <div class="model-note">
                <p>{selectedModel.description}</p>
                <div class="capabilities" aria-label="Model capabilities">
                  {#if selectedModel.capabilities.images}<span>Image</span>{/if}
                  {#if selectedModel.capabilities.video}<span>Video</span>{/if}
                  {#if selectedModel.capabilities.audioReferences}<span>Audio ref</span>{/if}
                  {#if selectedModel.capabilities.generatedAudio}<span>Soundtrack</span>{/if}
                </div>
              </div>
            {/if}

            <div class="settings-grid">
              <label class="field"><span>Duration</span><select value={snapshot.draft.settings.duration} onchange={(event) => editDraft((draft) => (draft.settings.duration = event.currentTarget.value))}><option value="">Provider default</option>{#each selectedModel?.durationOptions ?? [] as option}<option>{option}</option>{/each}</select></label>
              <label class="field"><span>Resolution</span><select value={snapshot.draft.settings.resolution} onchange={(event) => editDraft((draft) => (draft.settings.resolution = event.currentTarget.value))}><option value="">Provider default</option>{#each selectedModel?.resolutionOptions ?? [] as option}<option>{option}</option>{/each}</select></label>
              <label class="field"><span>Aspect ratio</span><select value={snapshot.draft.settings.aspectRatio} onchange={(event) => editDraft((draft) => (draft.settings.aspectRatio = event.currentTarget.value))}><option value="">Provider default</option>{#each selectedModel?.aspectRatioOptions ?? [] as option}<option>{option}</option>{/each}</select></label>
              <label class="field"><span>Generated audio</span><select disabled={!selectedModel?.capabilities.generatedAudio} value={snapshot.draft.settings.generatedAudio} onchange={(event) => editDraft((draft) => (draft.settings.generatedAudio = event.currentTarget.value as GenerationDraft['settings']['generatedAudio']))}><option value="provider_default">Provider default</option><option value="on">On</option><option value="off">Off</option></select></label>
              <label class="field field--wide"><span>Seed <small>optional</small></span><input inputmode="numeric" placeholder="Random" value={snapshot.draft.settings.seed} oninput={(event) => editDraft((draft) => (draft.settings.seed = event.currentTarget.value))} /></label>
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
      <section class="page-heading jobs-heading">
        <div><p class="eyebrow">Screening room</p><h1>Your films, taking shape.</h1><p>Watch each render grow, then play the finished cut.</p></div>
        <div class="job-summary"><strong>{activeJobCount}</strong><span>active {activeJobCount === 1 ? 'render' : 'renders'}</span></div>
      </section>

      <div class="jobs-layout">
        <aside class="jobs-sidebar" aria-label="Generation jobs">
          <div class="jobs-tools">
            <label class="search-field"><span class="sr-only">Search renders</span><Icon name="search" size={17} /><input bind:value={jobSearch} type="search" placeholder="Search renders" /></label>
            <label class="sr-only" for="job-filter">Filter jobs</label>
            <select id="job-filter" class="filter-select" bind:value={jobFilter}><option value="all">All</option><option value="active">Active</option><option value="attention">Needs attention</option><option value="completed">Completed</option></select>
          </div>
          {#if filteredJobs.length === 0}
            <div class="sidebar-empty"><Icon name="film" size={25} /><p>Nothing matches this reel.</p></div>
          {:else}
            <div class="job-list">
              {#each filteredJobs as job (job.id)}
                <button class:selected={snapshot.selectedJobId === job.id} class="job-row" onclick={() => void selectJob(job.id)}>
                  <span class={`status-orb status-orb--${statusTone(job.status)}`} aria-hidden="true"></span>
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
              <div><span class={`status-pill status-pill--${statusTone(selectedJob.status)}`}>{selectedJob.statusLabel}</span><h2>{selectedJob.prompt}</h2><p>{selectedJob.providerName} · {selectedJob.modelName}</p></div>
              <div class="detail-actions">
                {#if !selectedJob.deletable && selectedJob.status !== 'completed'}<button class="button button--secondary" onclick={() => void toggleJobMonitoring()}><Icon name={selectedJob.status === 'paused' ? 'play' : 'pause'} size={16} /> {selectedJob.status === 'paused' ? 'Resume updates' : 'Pause updates'}</button>{/if}
                {#if selectedJob.status === 'completed'}
                  <button class="button button--primary" disabled={outputBusy || playbackBusy} onclick={() => void playSelectedOutput()}><Icon name="play" size={16} /> {playbackBusy ? 'Loading…' : 'Play here'}</button>
                  <button class="button button--secondary" disabled={outputBusy || playbackBusy} onclick={() => void openSelectedOutput()}><Icon name="external" size={16} /> {outputBusy ? 'Opening…' : 'Open in player'}</button>
                {/if}
                {#if selectedJob.deletable}<button class="icon-button icon-button--danger" disabled={playbackBusy || deleteBusy} aria-label="Delete render" title="Delete render" onclick={askToDeleteRender}><Icon name="trash" size={17} /></button>{/if}
              </div>
            </div>

            {#if selectedJob.status === 'completed'}
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
              <CloudCinema active={isActiveJob(selectedJob.status)} paused={selectedJob.status === 'paused'} provider={selectedJob.providerName} status={selectedJob.statusLabel} detail={selectedJob.detail} jobId={selectedJob.id} elapsedSeconds={selectedJob.elapsedSeconds} nextPollSeconds={selectedJob.nextPollSeconds} />
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
              <div class="provider-actions"><button class="button button--primary" disabled={providerBusy === provider.id} type="submit">{providerBusy === provider.id ? 'Checking…' : provider.connected ? 'Replace key' : `Connect ${provider.name}`}</button>{#if provider.connected}<button class="button button--danger" disabled={providerBusy === provider.id} type="button" onclick={() => void forgetProvider(provider.id)}>Forget key</button>{/if}</div>
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
                <button class="button button--danger" type="button" disabled={!confirmedSafetyHolds[hold.handle] || holdBusy === hold.handle} onclick={() => void acknowledgeSafetyHold(hold.handle)}>{holdBusy === hold.handle ? 'Clearing hold…' : 'Dashboard checked — clear hold'}</button>
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

<dialog class="sheet-dialog review-dialog" bind:this={reviewDialog} aria-labelledby="review-title">
  {#if snapshot.preparedReview}
    {@const review = snapshot.preparedReview}
    <div class="dialog-accent" aria-hidden="true"></div>
    <div class="dialog-heading"><div class="dialog-icon"><Icon name="spark" size={22} /></div><div><p class="eyebrow">Final check</p><h2 id="review-title">One last look before the lights go down.</h2></div><button class="icon-button" aria-label="Close Review" onclick={() => reviewDialog.close()}><Icon name="x" size={18} /></button></div>
    <div class="review-price"><div><span>Fresh estimate</span><strong>{review.estimatedCost}</strong></div><p>Estimate only — your provider’s final usage is what counts.</p></div>
    <div class="dialog-body review-body">
      {#if review.uploadDisclosure}<div class="review-disclosure"><Icon name="warning" size={18} /><p>{review.uploadDisclosure}</p></div>{/if}
      <section><p class="micro-label">Prompt</p><p class="review-prompt">{review.prompt}</p></section>
      <div class="review-facts"><div><span>Provider</span><strong>{review.providerName}</strong></div><div><span>Model</span><strong title={review.modelName}>{review.modelName}</strong></div><div><span>Duration</span><strong>{review.settings.duration || 'Provider default'}</strong></div><div><span>Output</span><strong>{review.settings.resolution || 'Provider default'} · {review.settings.aspectRatio || 'Provider default'}</strong></div><div><span>Generated audio</span><strong>{review.settings.generatedAudio === 'on' ? 'On' : review.settings.generatedAudio === 'off' ? 'Off' : 'Provider default'}</strong></div><div><span>Seed</span><strong>{review.settings.seed || 'Random / provider default'}</strong></div></div>
      {#if review.advancedSettingsJson}<section class="review-advanced"><div><p class="micro-label">Extra model settings</p><p>These saved settings are included in this paid request.</p></div><pre>{review.advancedSettingsJson}</pre></section>{/if}
      {#if review.media.length > 0}<section><p class="micro-label">Reference media</p><ul class="review-media">{#each review.media as item (item.handle)}<li><Icon name={item.kind} size={16} /><span>{item.displayName}</span><small>{mediaRoleLabel(item.role)}</small></li>{/each}</ul></section>{/if}
      <p class="review-expiry"><Icon name="clock" size={15} /> Review expires at {formatDate(review.expiresAt)}. Any edit makes a fresh Review.</p>
    </div>
    <div class="dialog-actions dialog-actions--paid"><button class="button button--secondary" onclick={() => reviewDialog.close()}>Go back</button><div><span>Exactly one paid provider request</span><button class="button button--paid" disabled={isSubmitting} onclick={() => void submitReview()}>{isSubmitting ? 'Submitting once…' : 'Generate — one paid request'} <Icon name="spark" size={16} /></button></div></div>
  {/if}
</dialog>

{#if notice}
  <div class={`toast toast--${notice.tone}`} role={notice.tone === 'danger' ? 'alert' : 'status'}><span aria-hidden="true">{notice.tone === 'good' ? '✓' : notice.tone === 'danger' ? '!' : '◆'}</span><p>{notice.message}</p><button aria-label="Dismiss notification" onclick={() => (notice = null)}><Icon name="x" size={15} /></button></div>
{/if}

<div class="sr-only" aria-live="polite" aria-atomic="true">{liveMessage}</div>
