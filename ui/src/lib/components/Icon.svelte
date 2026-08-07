<script lang="ts">
  type IconName =
    | 'spark'
    | 'film'
    | 'plug'
    | 'paperclip'
    | 'plus'
    | 'check'
    | 'pause'
    | 'play'
    | 'external'
    | 'trash'
    | 'search'
    | 'key'
    | 'image'
    | 'video'
    | 'audio'
    | 'clock'
    | 'warning'
    | 'x'
    | 'chevron';

  let { name, size = 18 }: { name: IconName; size?: number } = $props();
</script>

<svg
  viewBox="0 0 24 24"
  width={size}
  height={size}
  fill="none"
  stroke="currentColor"
  stroke-width="1.8"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === 'spark'}
    <path d="m12 3 1.4 4.4L18 9l-4.6 1.6L12 15l-1.4-4.4L6 9l4.6-1.6L12 3Z" />
    <path d="m19 15 .7 2.3L22 18l-2.3.7L19 21l-.7-2.3L16 18l2.3-.7L19 15Z" />
  {:else if name === 'film'}
    <rect x="3" y="5" width="18" height="14" rx="2" />
    <path d="M7 5v14M17 5v14M3 9h4M17 9h4M3 15h4M17 15h4" />
  {:else if name === 'plug'}
    <path d="M9 7V3M15 7V3M7 7h10v3a5 5 0 0 1-10 0V7ZM12 15v6" />
  {:else if name === 'paperclip'}
    <path d="m20.5 11.5-8.8 8.8a6 6 0 0 1-8.5-8.5l9.2-9.2a4 4 0 0 1 5.7 5.7l-9.2 9.2a2 2 0 0 1-2.8-2.8l8.5-8.5" />
  {:else if name === 'plus'}
    <path d="M12 5v14M5 12h14" />
  {:else if name === 'check'}
    <path d="m5 12 4 4L19 6" />
  {:else if name === 'pause'}
    <path d="M9 5v14M15 5v14" />
  {:else if name === 'play'}
    <path d="m8 5 11 7-11 7V5Z" />
  {:else if name === 'external'}
    <path d="M14 4h6v6M20 4l-9 9" />
    <path d="M18 13v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h6" />
  {:else if name === 'trash'}
    <path d="M4 7h16M9 7V4h6v3M7 7l1 13h8l1-13M10 11v5M14 11v5" />
  {:else if name === 'search'}
    <circle cx="11" cy="11" r="7" />
    <path d="m20 20-4-4" />
  {:else if name === 'key'}
    <circle cx="8" cy="15" r="4" />
    <path d="m11 12 8-8M16 7l2 2M14 9l2 2" />
  {:else if name === 'image'}
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <circle cx="9" cy="10" r="2" />
    <path d="m4 18 5-5 4 4 3-3 4 4" />
  {:else if name === 'video'}
    <rect x="3" y="6" width="13" height="12" rx="2" />
    <path d="m16 10 5-3v10l-5-3" />
  {:else if name === 'audio'}
    <path d="M9 18V6l10-2v12" />
    <circle cx="6" cy="18" r="3" />
    <circle cx="16" cy="16" r="3" />
  {:else if name === 'clock'}
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7v5l3 2" />
  {:else if name === 'warning'}
    <path d="M12 3 2.8 20h18.4L12 3Z" />
    <path d="M12 9v4M12 17h.01" />
  {:else if name === 'x'}
    <path d="m6 6 12 12M18 6 6 18" />
  {:else if name === 'chevron'}
    <path d="m9 6 6 6-6 6" />
  {/if}
</svg>
