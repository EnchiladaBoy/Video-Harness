import { modelById } from './types';
import type {
  AppSnapshot,
  GenerationDraft,
  MediaItem,
  MediaKind,
  MediaRole,
  ModelSummary
} from './types';

export const MAX_MEDIA_ITEMS = 32;

export const IMAGE_ROLE_OPTIONS: Array<{ value: MediaRole; label: string }> = [
  { value: 'reference', label: 'Reference' },
  { value: 'start_frame', label: 'Start frame' },
  { value: 'end_frame', label: 'End frame' }
];

const CREDENTIAL_FIELD_NAMES = new Set([
  'apikey',
  'authorization',
  'credential',
  'credentials',
  'secret',
  'token',
  'accesstoken',
  'refreshtoken',
  'clientsecret',
  'secretkey',
  'privatekey',
  'password',
  'passwd',
  'authtoken',
  'bearertoken',
  'falkey'
]);

const ALLOWED_TOKEN_CONTROL_NAMES = new Set([
  'maxtoken',
  'mintoken',
  'numtoken',
  'tokencount',
  'tokenlimit'
]);

function mediaKindLabel(kind: MediaKind): string {
  if (kind === 'image') return 'image';
  if (kind === 'video') return 'video';
  return 'audio';
}

export function imageRoleOptions(model?: ModelSummary): Array<{ value: MediaRole; label: string }> {
  const supported = model?.supportedImageRoles;
  if (!supported) return IMAGE_ROLE_OPTIONS;
  return IMAGE_ROLE_OPTIONS.filter((option) => supported.includes(option.value));
}

export function modelSupportsImages(model?: ModelSummary): boolean {
  return Boolean(
    model &&
      (model.capabilities.images || (model.supportedImageRoles?.length ?? 0) > 0)
  );
}

export interface MediaAppendResult {
  accepted: MediaItem[];
  skipped: number;
}

function modelSupportsMediaKind(model: ModelSummary, kind: MediaKind): boolean {
  if (kind === 'image') return modelSupportsImages(model);
  if (kind === 'video') return model.capabilities.video;
  return model.capabilities.audioReferences;
}

function mediaMatchesConstraint(
  item: Pick<MediaItem, 'kind' | 'role'>,
  constraint: NonNullable<ModelSummary['mediaConstraints']>[number]
): boolean {
  return (
    item.kind === constraint.kind &&
    (!constraint.roles || constraint.roles.includes(item.role))
  );
}

function candidateFitsMediaLimits(
  media: MediaItem[],
  candidate: MediaItem,
  model?: ModelSummary
): boolean {
  const modelLimit =
    model?.maxMediaItems !== undefined && model.maxMediaItems >= 0
      ? model.maxMediaItems
      : MAX_MEDIA_ITEMS;
  if (media.length >= Math.min(MAX_MEDIA_ITEMS, modelLimit)) return false;

  if (
    candidate.kind === 'image' &&
    (candidate.role === 'start_frame' || candidate.role === 'end_frame') &&
    media.some((item) => item.kind === 'image' && item.role === candidate.role)
  ) {
    return false;
  }

  return (model?.mediaConstraints ?? []).every((constraint) => {
    if (!mediaMatchesConstraint(candidate, constraint) || constraint.maxItems === undefined) {
      return true;
    }
    const count = media.filter((item) => mediaMatchesConstraint(item, constraint)).length;
    return count < constraint.maxItems;
  });
}

/**
 * Fits newly selected media into the renderer-visible model limits before it
 * reaches the draft. Local image pickers return the generic reference role;
 * for a frame-only model, allocate the missing frame roles in catalog order.
 */
export function constrainMediaAppend(
  existing: MediaItem[],
  incoming: MediaItem[],
  model?: ModelSummary
): MediaAppendResult {
  const accepted: MediaItem[] = [];
  const working = existing.map((item) => ({ ...item }));

  for (const original of incoming) {
    if (model && !modelSupportsMediaKind(model, original.kind)) continue;

    let candidate: MediaItem | undefined;
    if (original.kind === 'video') {
      const normalized = { ...original, role: 'video_reference' as const };
      if (candidateFitsMediaLimits(working, normalized, model)) candidate = normalized;
    } else if (original.kind === 'audio') {
      const normalized = { ...original, role: 'audio_reference' as const };
      if (candidateFitsMediaLimits(working, normalized, model)) candidate = normalized;
    } else {
      const supportedRoles = imageRoleOptions(model).map((option) => option.value);
      const requestedIsSupported = supportedRoles.includes(original.role);
      const missingRequiredRoles = (model?.requiredImageRoles ?? []).filter(
        (role) =>
          supportedRoles.includes(role) &&
          !working.some((item) => item.kind === 'image' && item.role === role)
      );
      const roleCandidates = requestedIsSupported
        ? [original.role]
        : [...new Set([...missingRequiredRoles, ...supportedRoles])];
      for (const role of roleCandidates) {
        const normalized = { ...original, role };
        if (candidateFitsMediaLimits(working, normalized, model)) {
          candidate = normalized;
          break;
        }
      }
    }

    if (!candidate) continue;
    accepted.push(candidate);
    working.push(candidate);
  }

  return { accepted, skipped: incoming.length - accepted.length };
}

function isCredentialFieldName(name: string): boolean {
  const normalized = name.replace(/[^a-z0-9]/gi, '').toLowerCase();
  if (CREDENTIAL_FIELD_NAMES.has(normalized)) return true;

  return (
    normalized.startsWith('authorization') ||
    normalized.endsWith('apikey') ||
    normalized.endsWith('secret') ||
    (normalized.endsWith('token') && !ALLOWED_TOKEN_CONTROL_NAMES.has(normalized))
  );
}

function containsCredentialField(value: unknown): boolean {
  const pending: unknown[] = [value];
  while (pending.length > 0) {
    const current = pending.pop();
    if (Array.isArray(current)) {
      for (const nested of current) pending.push(nested);
    } else if (current && typeof current === 'object') {
      for (const [key, nested] of Object.entries(current)) {
        if (isCredentialFieldName(key)) return true;
        pending.push(nested);
      }
    }
  }
  return false;
}

export function advancedJsonIssue(value: string | undefined): string | undefined {
  const trimmed = value?.trim() ?? '';
  if (!trimmed) return undefined;
  if (value && value.length > 100_000) return 'Advanced settings are too large.';
  try {
    const parsed: unknown = JSON.parse(trimmed);
    if (!parsed || Array.isArray(parsed) || typeof parsed !== 'object') {
      return 'Advanced settings must be a JSON object.';
    }
    if (containsCredentialField(parsed)) {
      return 'Advanced settings may not contain credential fields.';
    }
  } catch {
    return 'Fix the JSON in Advanced settings.';
  }
  return undefined;
}

function unsupportedOption(
  value: string | undefined,
  options: string[] | undefined,
  label: string
): string | undefined {
  if (!value || options === undefined || options.includes(value)) return undefined;
  return `Choose a supported ${label}; the saved value is no longer in this model’s catalog.`;
}

export function reviewReadinessIssue(snapshot: AppSnapshot, ready: boolean): string | undefined {
  if (!ready) return 'Video Harness is still opening.';
  const provider = snapshot.providers.find((item) => item.id === snapshot.draft.providerId);
  if (!provider?.connected) return `Connect ${provider?.name ?? 'a video service'} under Connections.`;
  if (!snapshot.draft.prompt.trim()) return 'Add your idea above.';

  const model = modelById(snapshot, snapshot.draft.providerId, snapshot.draft.modelId);
  if (!model) {
    const hasProviderModels = snapshot.models.some(
      (item) => item.providerId === snapshot.draft.providerId
    );
    return hasProviderModels ? 'Choose a video model.' : 'No models are available for this service yet.';
  }

  if (snapshot.draft.media.length > MAX_MEDIA_ITEMS) {
    return `Use at most ${MAX_MEDIA_ITEMS} reference items.`;
  }
  if (
    model.maxMediaItems !== undefined &&
    model.maxMediaItems >= 0 &&
    snapshot.draft.media.length > model.maxMediaItems
  ) {
    return model.maxMediaItems === 1
      ? 'This model accepts at most one reference item in total.'
      : `This model accepts at most ${model.maxMediaItems} reference items in total.`;
  }

  for (const item of snapshot.draft.media) {
    const supported =
      item.kind === 'image'
        ? modelSupportsImages(model)
        : item.kind === 'video'
          ? model.capabilities.video
          : model.capabilities.audioReferences;
    if (!supported) return `This model can’t use ${mediaKindLabel(item.kind)} references.`;
    if (item.kind === 'video' && item.role !== 'video_reference') {
      return `Choose the video reference role for ${item.displayName}.`;
    }
    if (item.kind === 'audio' && item.role !== 'audio_reference') {
      return `Choose the audio reference role for ${item.displayName}.`;
    }
    if (
      item.kind === 'image' &&
      model.supportedImageRoles &&
      !model.supportedImageRoles.includes(item.role)
    ) {
      return `Choose a supported role for ${item.displayName}.`;
    }
  }

  for (const role of ['start_frame', 'end_frame'] as const) {
    const matchingFrames = snapshot.draft.media.filter(
      (item) => item.kind === 'image' && item.role === role
    );
    if (matchingFrames.length > 1) {
      return role === 'start_frame'
        ? 'Use at most one start frame.'
        : 'Use at most one end frame.';
    }
  }

  for (const role of model.requiredImageRoles ?? []) {
    if (role !== 'reference' && role !== 'start_frame' && role !== 'end_frame') continue;
    const present = snapshot.draft.media.some(
      (item) => item.kind === 'image' && item.role === role
    );
    if (!present) {
      if (role === 'start_frame') return 'This model requires a start frame.';
      if (role === 'end_frame') return 'This model requires an end frame.';
      return 'This model requires a general image reference.';
    }
  }

  if (model.audioRequiresVisual) {
    const hasAudio = snapshot.draft.media.some((item) => item.kind === 'audio');
    const hasVisual = snapshot.draft.media.some(
      (item) => item.kind === 'image' || item.kind === 'video'
    );
    if (hasAudio && !hasVisual) {
      return 'Audio references for this model require at least one image or video reference.';
    }
  }

  if (model.framesExclusiveWithReferences) {
    const hasFrame = snapshot.draft.media.some(
      (item) =>
        item.kind === 'image' && (item.role === 'start_frame' || item.role === 'end_frame')
    );
    const hasInputReference = snapshot.draft.media.some(
      (item) =>
        item.role === 'reference' ||
        item.role === 'video_reference' ||
        item.role === 'audio_reference'
    );
    if (hasFrame && hasInputReference) {
      return 'Use either frame images or non-frame input references with this model, not both.';
    }
  }

  for (const constraint of model.mediaConstraints ?? []) {
    const count = snapshot.draft.media.filter(
      (item) =>
        item.kind === constraint.kind &&
        (!constraint.roles || constraint.roles.includes(item.role))
    ).length;
    const minimum = constraint.minItems ?? (constraint.required ? 1 : 0);
    if (count < minimum) {
      return minimum === 1
        ? `This model requires ${constraint.kind === 'video' ? 'a' : 'an'} ${mediaKindLabel(constraint.kind)} reference.`
        : `This model requires at least ${minimum} ${mediaKindLabel(constraint.kind)} references.`;
    }
    const conditionalMinimum = constraint.minItemsWhenPresent;
    if (count > 0 && conditionalMinimum !== undefined && count < conditionalMinimum) {
      return conditionalMinimum === 1
        ? `This model requires ${constraint.kind === 'video' ? 'a' : 'an'} ${mediaKindLabel(constraint.kind)} reference when that media type is used.`
        : `This model requires at least ${conditionalMinimum} ${mediaKindLabel(constraint.kind)} references when that media type is used.`;
    }
    if (constraint.maxItems !== undefined && count > constraint.maxItems) {
      return constraint.maxItems === 1
        ? `This model accepts at most one ${mediaKindLabel(constraint.kind)} reference.`
        : `This model accepts at most ${constraint.maxItems} ${mediaKindLabel(constraint.kind)} references.`;
    }
  }

  const settings = snapshot.draft.settings;
  if (settings.size && (settings.resolution || settings.aspectRatio)) {
    return 'Choose either an exact output size or resolution and aspect ratio, not both.';
  }
  const optionIssue =
    unsupportedOption(settings.duration, model.durationOptions, 'duration') ??
    unsupportedOption(settings.resolution, model.resolutionOptions, 'resolution') ??
    unsupportedOption(settings.aspectRatio, model.aspectRatioOptions, 'aspect ratio') ??
    unsupportedOption(settings.size, model.sizeOptions, 'output size');
  if (optionIssue) return optionIssue;
  if (!model.capabilities.generatedAudio && settings.generatedAudio !== 'provider_default') {
    return 'Set generated audio to Provider default for this model.';
  }
  if (model.capabilities.seed === false && settings.seed.trim()) {
    return 'Clear the seed; this model does not support seeded generation.';
  }
  const seed = settings.seed.trim();
  if (seed && !/^-?\d+$/.test(seed)) return 'Seed must be a whole number.';
  if (seed) {
    try {
      const parsed = BigInt(seed);
      if (parsed < -(2n ** 63n) || parsed > 2n ** 63n - 1n) {
        return 'Seed must fit in a signed 64-bit whole number.';
      }
    } catch {
      return 'Seed must be a whole number.';
    }
  }
  const jsonIssue = advancedJsonIssue(settings.advancedJson);
  if (jsonIssue) return jsonIssue;

  const requiresOpenRouterUpload =
    snapshot.draft.providerId === 'openrouter' &&
    snapshot.draft.media.some((item) => item.source === 'local');
  const falProvider = snapshot.providers.find((item) => item.id === 'fal');
  if (requiresOpenRouterUpload && !falProvider?.connected) {
    return 'Connect fal.ai under Connections to upload these files for OpenRouter.';
  }
  return undefined;
}

export function normalizeDraftForModel(
  draft: GenerationDraft,
  model: ModelSummary | undefined,
  options: { chooseDefaults?: boolean; clearAdvanced?: boolean } = {}
): void {
  if (!model) {
    draft.settings.duration = '';
    draft.settings.resolution = '';
    draft.settings.aspectRatio = '';
    draft.settings.size = '';
    draft.settings.generatedAudio = 'provider_default';
    draft.settings.seed = '';
    if (options.clearAdvanced) draft.settings.advancedJson = '';
    return;
  }
  if (options.chooseDefaults) {
    const defaultSize = model.sizeOptions?.[0] ?? '';
    draft.settings.duration = model.durationOptions[0] ?? '';
    draft.settings.size = defaultSize;
    draft.settings.resolution = defaultSize ? '' : (model.resolutionOptions[0] ?? '');
    draft.settings.aspectRatio = defaultSize ? '' : (model.aspectRatioOptions[0] ?? '');
  }
  if (draft.settings.size) {
    draft.settings.resolution = '';
    draft.settings.aspectRatio = '';
  }
  if (!model.capabilities.generatedAudio) draft.settings.generatedAudio = 'provider_default';
  if (model.capabilities.seed === false) draft.settings.seed = '';
  if (options.clearAdvanced) draft.settings.advancedJson = '';
}
