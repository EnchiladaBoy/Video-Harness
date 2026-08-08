<script lang="ts">
  import { onMount } from 'svelte';

  interface Props {
    active: boolean;
    paused?: boolean;
    provider: string;
    status: string;
    detail?: string;
    jobId?: string;
    elapsedSeconds?: number;
    nextPollSeconds?: number;
  }

  let {
    active,
    paused = false,
    provider,
    status,
    detail,
    jobId,
    elapsedSeconds,
    nextPollSeconds
  }: Props = $props();

  let canvas: HTMLCanvasElement;
  let inViewport = $state(true);
  let pageVisible = $state(true);
  let reducedMotion = $state(false);
  let phase = 0;

  const FRAME_INTERVAL = 240;

  function formatDuration(value?: number): string {
    if (value === undefined) return '';
    const minutes = Math.floor(value / 60);
    const seconds = value % 60;
    return minutes > 0 ? `${minutes}m ${seconds.toString().padStart(2, '0')}s` : `${seconds}s`;
  }

  function color(variable: string, fallback: string): string {
    if (typeof document === 'undefined') return fallback;
    return getComputedStyle(document.documentElement).getPropertyValue(variable).trim() || fallback;
  }

  function pixelRect(
    context: CanvasRenderingContext2D,
    x: number,
    y: number,
    width: number,
    height: number,
    fill: string
  ): void {
    context.fillStyle = fill;
    context.fillRect(Math.round(x), Math.round(y), Math.round(width), Math.round(height));
  }

  function drawReel(
    context: CanvasRenderingContext2D,
    centerX: number,
    centerY: number,
    radius: number,
    direction: number,
    palette: { ink: string; cyan: string; panel: string }
  ): void {
    context.fillStyle = palette.panel;
    context.strokeStyle = palette.cyan;
    context.lineWidth = Math.max(2, radius / 6);
    context.beginPath();
    context.arc(centerX, centerY, radius, 0, Math.PI * 2);
    context.fill();
    context.stroke();

    const spokePhase = ((phase % 4) * Math.PI) / 2 * direction;
    context.strokeStyle = palette.ink;
    context.lineWidth = Math.max(1, radius / 9);
    for (let index = 0; index < 4; index += 1) {
      const angle = spokePhase + (index * Math.PI) / 2;
      context.beginPath();
      context.moveTo(centerX, centerY);
      context.lineTo(
        centerX + Math.cos(angle) * radius * 0.68,
        centerY + Math.sin(angle) * radius * 0.68
      );
      context.stroke();
    }
    pixelRect(context, centerX - 2, centerY - 2, 4, 4, palette.cyan);
  }

  function draw(): void {
    if (!canvas) return;
    const bounds = canvas.getBoundingClientRect();
    if (bounds.width <= 0 || bounds.height <= 0) return;

    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    const targetWidth = Math.round(bounds.width * ratio);
    const targetHeight = Math.round(bounds.height * ratio);
    if (canvas.width !== targetWidth || canvas.height !== targetHeight) {
      canvas.width = targetWidth;
      canvas.height = targetHeight;
    }

    const context = canvas.getContext('2d');
    if (!context) return;
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    context.imageSmoothingEnabled = false;

    const width = bounds.width;
    const height = bounds.height;
    const scale = Math.min(width / 540, height / 230);
    const palette = {
      sky: color('--cinema-sky', '#091525'),
      skyGlow: color('--cinema-glow', '#17304b'),
      panel: color('--cinema-panel', '#111d2d'),
      ink: color('--cinema-ink', '#eef5f4'),
      muted: color('--cinema-muted', '#6f8394'),
      cyan: color('--accent', '#62e0d1'),
      coral: color('--accent-warm', '#ff9a74'),
      cloud: color('--cinema-cloud', '#9ad6dc')
    };

    context.clearRect(0, 0, width, height);
    context.fillStyle = palette.sky;
    context.fillRect(0, 0, width, height);

    const glow = context.createRadialGradient(width * 0.5, height * 0.78, 4, width * 0.5, height * 0.7, width * 0.52);
    glow.addColorStop(0, `${palette.skyGlow}f2`);
    glow.addColorStop(1, `${palette.sky}00`);
    context.fillStyle = glow;
    context.fillRect(0, 0, width, height);

    const starSlots = [0.08, 0.23, 0.39, 0.58, 0.76, 0.91];
    for (let index = 0; index < starSlots.length; index += 1) {
      const direction = index % 2 === 0 ? -1 : 1;
      const shift = active ? ((phase * direction + index * 7) % 38) / 380 : 0;
      const x = ((starSlots[index] + shift + 1) % 1) * width;
      const y = (0.12 + ((index * 0.113) % 0.27)) * height;
      const size = (index % 3 === 0 ? 3 : 2) * Math.max(scale, 0.72);
      pixelRect(context, x - size, y, size * 2, Math.max(1, size / 2), palette.coral);
      pixelRect(context, x - size / 4, y - size * 0.75, Math.max(1, size / 2), size * 2, palette.coral);
    }

    const cloudRows = [0.31, 0.43, 0.36, 0.48];
    for (let index = 0; index < cloudRows.length; index += 1) {
      const speed = 58 + index * 17;
      const slot = ((index * 0.29 + phase / speed) % 1) * (width + 80) - 40;
      const y = cloudRows[index] * height;
      const block = Math.max(4, 7 * scale);
      context.globalAlpha = index % 2 === 0 ? 0.48 : 0.27;
      for (const [column, row] of [
        [0, 1],
        [1, 0],
        [1, 1],
        [2, 0],
        [2, 1],
        [3, 1],
        [4, 1]
      ]) {
        pixelRect(context, slot + column * block, y + row * block, block, block, palette.cloud);
      }
    }
    context.globalAlpha = 1;

    const bodyWidth = Math.min(205 * scale, width * 0.58);
    const bodyHeight = Math.max(38, 48 * scale);
    const bodyX = width / 2 - bodyWidth / 2;
    const bodyY = height * 0.63;
    const beam = context.createLinearGradient(width / 2, bodyY, width / 2, height);
    beam.addColorStop(0, `${palette.cyan}30`);
    beam.addColorStop(1, `${palette.cyan}00`);
    context.fillStyle = beam;
    context.beginPath();
    context.moveTo(bodyX + 16, bodyY + bodyHeight * 0.35);
    context.lineTo(bodyX - 54 * scale, height);
    context.lineTo(bodyX + bodyWidth + 54 * scale, height);
    context.lineTo(bodyX + bodyWidth - 16, bodyY + bodyHeight * 0.35);
    context.closePath();
    context.fill();

    pixelRect(context, bodyX, bodyY, bodyWidth, bodyHeight, palette.panel);
    context.strokeStyle = palette.ink;
    context.lineWidth = Math.max(1, 2 * scale);
    context.strokeRect(Math.round(bodyX), Math.round(bodyY), Math.round(bodyWidth), Math.round(bodyHeight));

    const reelRadius = Math.max(15, 20 * scale);
    const reelY = bodyY - reelRadius * 0.48;
    drawReel(context, width / 2 - bodyWidth * 0.25, reelY, reelRadius, 1, palette);
    drawReel(context, width / 2 + bodyWidth * 0.25, reelY, reelRadius, -1, palette);

    pixelRect(context, bodyX + bodyWidth * 0.1, bodyY + bodyHeight * 0.25, 7 * scale, 7 * scale, palette.coral);
    pixelRect(context, bodyX + bodyWidth * 0.1, bodyY + bodyHeight * 0.62, 7 * scale, 4 * scale, palette.cyan);

    context.fillStyle = palette.ink;
    context.font = `700 ${Math.max(8, 10 * scale)}px ui-monospace, SFMono-Regular, Menlo, monospace`;
    context.textAlign = 'center';
    context.textBaseline = 'middle';
    context.fillText('VIDEO HARNESS', width / 2 + bodyWidth * 0.05, bodyY + bodyHeight * 0.54);

    const groundY = height - Math.max(12, 17 * scale);
    for (let x = width / 2 - bodyWidth * 0.72; x < width / 2 + bodyWidth * 0.72; x += 12 * scale) {
      pixelRect(context, x, groundY, 6 * scale, Math.max(1, 2 * scale), palette.coral);
    }
  }

  onMount(() => {
    pageVisible = document.visibilityState === 'visible';
    const motionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
    reducedMotion = motionQuery.matches;
    const onMotionChange = (event: MediaQueryListEvent) => (reducedMotion = event.matches);
    const onVisibilityChange = () => (pageVisible = document.visibilityState === 'visible');
    motionQuery.addEventListener('change', onMotionChange);
    document.addEventListener('visibilitychange', onVisibilityChange);

    const intersection = new IntersectionObserver(
      ([entry]) => (inViewport = entry?.isIntersecting ?? true),
      { threshold: 0.05 }
    );
    intersection.observe(canvas);
    const resize = new ResizeObserver(draw);
    resize.observe(canvas);
    draw();

    return () => {
      motionQuery.removeEventListener('change', onMotionChange);
      document.removeEventListener('visibilitychange', onVisibilityChange);
      intersection.disconnect();
      resize.disconnect();
    };
  });

  $effect(() => {
    const shouldAnimate = active && !paused && inViewport && pageVisible && !reducedMotion;
    provider;
    status;
    detail;
    draw();
    if (!shouldAnimate) return;

    const timer = window.setInterval(() => {
      phase = (phase + 1) % 10_000;
      draw();
    }, FRAME_INTERVAL);
    return () => window.clearInterval(timer);
  });
</script>

<section class:paused class="cinema" aria-labelledby="cloud-cinema-status">
  <div class="cinema__topline">
    <span class="eyebrow">{provider}</span>
    <span class="live-dot" class:is-active={active && !paused} aria-hidden="true"></span>
  </div>
  <canvas bind:this={canvas} class="cinema__canvas" aria-hidden="true"></canvas>
  <div class="cinema__telemetry">
    <div>
      <p class="micro-label">Current status</p>
      <h3 id="cloud-cinema-status">{status}</h3>
      {#if detail}<p class="cinema__detail">{detail}</p>{/if}
    </div>
    {#if elapsedSeconds !== undefined || nextPollSeconds !== undefined}
      <p class="cinema__timing">
        {#if elapsedSeconds !== undefined}<span>{formatDuration(elapsedSeconds)} elapsed</span>{/if}
        {#if nextPollSeconds !== undefined}<span>Next check in {formatDuration(nextPollSeconds)}</span>{/if}
      </p>
    {/if}
    {#if jobId}<code class="cinema__job">Job {jobId}</code>{/if}
  </div>
</section>

<style>
  .cinema {
    --cinema-sky: #091525;
    --cinema-glow: #17304b;
    --cinema-panel: #111d2d;
    --cinema-ink: #eef5f4;
    --cinema-muted: #6f8394;
    --cinema-cloud: #9ad6dc;
    overflow: hidden;
    border: 1px solid color-mix(in srgb, var(--accent) 24%, var(--border));
    border-radius: var(--radius-xl);
    background: var(--surface-raised);
    box-shadow: var(--shadow-card);
  }

  .cinema.paused {
    border-color: color-mix(in srgb, var(--warning) 48%, var(--border));
  }

  .cinema__topline {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 1rem 1.15rem 0;
  }

  .live-dot {
    width: 0.52rem;
    height: 0.52rem;
    border-radius: 50%;
    background: var(--text-faint);
  }

  .live-dot.is-active {
    background: var(--accent);
    box-shadow: 0 0 0 0.28rem color-mix(in srgb, var(--accent) 14%, transparent);
  }

  .cinema__canvas {
    display: block;
    width: calc(100% - 1.5rem);
    height: clamp(12rem, 28vw, 15rem);
    margin: 0.65rem 0.75rem 0;
    border-radius: calc(var(--radius-xl) - 0.5rem);
  }

  .cinema__telemetry {
    display: grid;
    gap: 0.8rem;
    padding: 1.05rem 1.2rem 1.2rem;
  }

  .cinema h3 {
    margin: 0.18rem 0 0;
    font-size: clamp(1.05rem, 2vw, 1.28rem);
    letter-spacing: -0.02em;
  }

  .cinema__detail {
    margin: 0.38rem 0 0;
    color: var(--text-muted);
    line-height: 1.45;
  }

  .cinema__timing {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem 1rem;
    margin: 0;
    color: var(--text-muted);
    font: 0.75rem/1.4 var(--font-mono);
  }

  .cinema__job {
    overflow: hidden;
    color: var(--text-faint);
    font-size: 0.72rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
