import { act, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import App from './App.svelte';
import { createMockBridge, demoSnapshot } from './lib/mock-bridge';
import type { OpenSessionResult, UiEventEnvelope } from './lib/types';

describe('creative draft input', () => {
  it('captures prompt typing and marks the draft for autosave', async () => {
    render(App);
    const prompt = screen.getByLabelText('Video prompt');
    await waitFor(() => expect(prompt).not.toBeDisabled());

    const idea = 'A shy moon waves from a paper sky.';
    await fireEvent.input(prompt, { target: { value: idea } });

    expect(prompt).toHaveValue(idea);
    expect(screen.getByText(`${idea.length} / 8,000`)).toBeInTheDocument();
    expect(screen.getByText('Saving…')).toBeInTheDocument();
  });

  it('keeps exact size mutually exclusive with resolution and aspect controls', async () => {
    render(App);
    const size = screen.getByLabelText('Output size');
    const resolution = screen.getByLabelText('Resolution');
    const aspect = screen.getByLabelText('Aspect ratio');

    await fireEvent.change(size, { target: { value: '1280x720' } });
    expect(size).toHaveValue('1280x720');
    expect(resolution).toHaveValue('');
    expect(aspect).toHaveValue('');

    await fireEvent.change(resolution, { target: { value: '720p' } });
    expect(resolution).toHaveValue('720p');
    expect(size).toHaveValue('');

    await fireEvent.change(size, { target: { value: '1280x720' } });
    await fireEvent.change(aspect, { target: { value: '9:16' } });
    expect(aspect).toHaveValue('9:16');
    expect(size).toHaveValue('');
  });

  it('acknowledges buffered close edges and safely retains failed cancellations', async () => {
    const bridge = createMockBridge();
    Object.defineProperty(bridge, 'mode', { value: 'tauri' });
    let receiveEvent: ((event: UiEventEnvelope) => void) | undefined;
    let resolveSession: ((session: OpenSessionResult) => void) | undefined;
    const session = new Promise<OpenSessionResult>((resolve) => {
      resolveSession = resolve;
    });
    vi.spyOn(bridge, 'openSession').mockImplementation((onEvent) => {
      receiveEvent = onEvent;
      return session;
    });
    const acknowledge = vi.spyOn(bridge, 'acknowledgeCloseRequest');
    const saveAndClose = vi
      .spyOn(bridge, 'saveDraftAndClose')
      .mockRejectedValue(new Error('The fixture save was refused.'));
    const cancel = vi
      .spyOn(bridge, 'cancelCloseRequest')
      .mockRejectedValueOnce(new Error('native-secret-that-must-not-render'))
      .mockResolvedValue(undefined);

    render(App, { bridge });
    await waitFor(() => expect(receiveEvent).toBeDefined());
    await act(() =>
      receiveEvent?.({ seq: 8, event: { type: 'close_requested', requestId: 73 } })
    );

    expect(acknowledge).toHaveBeenCalledTimes(1);
    expect(acknowledge).toHaveBeenCalledWith(73);
    expect(saveAndClose).not.toHaveBeenCalled();

    await act(() =>
      resolveSession?.({
        seq: 7,
        snapshot: demoSnapshot(),
        preparing: false,
        submitting: false
      })
    );
    const dialog = await screen.findByRole('dialog', { name: 'Save this scene safely?' });
    await waitFor(() =>
      expect(saveAndClose).toHaveBeenCalledWith(expect.any(Object), 73)
    );

    await fireEvent.click(screen.getByRole('button', { name: 'Keep working' }));
    expect(await screen.findByText('The close request is still active.')).toBeInTheDocument();
    expect(screen.queryByText(/native-secret-that-must-not-render/)).not.toBeInTheDocument();
    expect(dialog).toHaveAttribute('open');

    await fireEvent.click(screen.getByRole('button', { name: 'Retry Keep working' }));
    await waitFor(() => expect(dialog).not.toHaveAttribute('open'));
    expect(cancel).toHaveBeenNthCalledWith(1, 73);
    expect(cancel).toHaveBeenNthCalledWith(2, 73);
  });

  it('hands a newer close request across an older cancellation', async () => {
    const bridge = createMockBridge();
    Object.defineProperty(bridge, 'mode', { value: 'tauri' });
    let receiveEvent: ((event: UiEventEnvelope) => void) | undefined;
    vi.spyOn(bridge, 'openSession').mockImplementation(async (onEvent) => {
      receiveEvent = onEvent;
      return {
        seq: 0,
        snapshot: demoSnapshot(),
        preparing: false,
        submitting: false
      };
    });
    const acknowledge = vi.spyOn(bridge, 'acknowledgeCloseRequest');
    const saveAndClose = vi
      .spyOn(bridge, 'saveDraftAndClose')
      .mockRejectedValue(new Error('Keep the fixture window open.'));
    let resolveCancellation: (() => void) | undefined;
    vi.spyOn(bridge, 'cancelCloseRequest').mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveCancellation = resolve;
        })
    );

    render(App, { bridge });
    await waitFor(() => expect(receiveEvent).toBeDefined());
    await act(() =>
      receiveEvent?.({ seq: 1, event: { type: 'close_requested', requestId: 80 } })
    );
    await waitFor(() => expect(saveAndClose).toHaveBeenCalledWith(expect.any(Object), 80));

    await fireEvent.click(screen.getByRole('button', { name: 'Keep working' }));
    await waitFor(() => expect(resolveCancellation).toBeDefined());
    await act(() =>
      receiveEvent?.({ seq: 2, event: { type: 'close_requested', requestId: 81 } })
    );
    expect(acknowledge).toHaveBeenCalledWith(81);

    await act(() => resolveCancellation?.());
    await waitFor(() => expect(saveAndClose).toHaveBeenCalledWith(expect.any(Object), 81));
  });

  it('starts a second snapshot resync when buffered replay exposes another gap', async () => {
    const bridge = createMockBridge();
    let receiveEvent: ((event: UiEventEnvelope) => void) | undefined;
    vi.spyOn(bridge, 'openSession').mockImplementation(async (onEvent) => {
      receiveEvent = onEvent;
      return {
        seq: 0,
        snapshot: demoSnapshot(),
        preparing: false,
        submitting: false
      };
    });
    let resolveFirstSnapshot: ((session: OpenSessionResult) => void) | undefined;
    const firstSnapshot = new Promise<OpenSessionResult>((resolve) => {
      resolveFirstSnapshot = resolve;
    });
    const getSnapshot = vi
      .spyOn(bridge, 'getSnapshot')
      .mockImplementationOnce(() => firstSnapshot)
      .mockResolvedValueOnce({
        seq: 4,
        snapshot: demoSnapshot(),
        preparing: false,
        submitting: false
      });

    render(App, { bridge });
    await waitFor(() => expect(receiveEvent).toBeDefined());
    await act(() =>
      receiveEvent?.({
        seq: 2,
        event: { type: 'notice', tone: 'neutral', message: 'First gap' }
      })
    );
    await waitFor(() => expect(getSnapshot).toHaveBeenCalledTimes(1));
    await act(() =>
      receiveEvent?.({
        seq: 4,
        event: { type: 'notice', tone: 'neutral', message: 'Second gap' }
      })
    );
    await act(() =>
      resolveFirstSnapshot?.({
        seq: 2,
        snapshot: demoSnapshot(),
        preparing: false,
        submitting: false
      })
    );

    await waitFor(() => expect(getSnapshot).toHaveBeenCalledTimes(2));
  });

  it('keeps native preparation busy across routine catalog and history snapshots', async () => {
    const bridge = createMockBridge();
    let receiveEvent: ((event: UiEventEnvelope) => void) | undefined;
    vi.spyOn(bridge, 'openSession').mockImplementation(async (onEvent) => {
      receiveEvent = onEvent;
      return {
        seq: 0,
        snapshot: demoSnapshot(),
        preparing: true,
        submitting: false
      };
    });

    render(App, { bridge });
    const preparing = await screen.findByRole('button', { name: 'Preparing Review…' });
    expect(preparing).toBeDisabled();
    expect(screen.getByText('Model catalog')).toBeInTheDocument();

    const routineSnapshot = demoSnapshot();
    routineSnapshot.jobs = [
      {
        ...routineSnapshot.jobs[0],
        id: 'history-loaded-after-open',
        prompt: 'A render loaded from ordinary history.'
      },
      ...routineSnapshot.jobs
    ];
    await act(() =>
      receiveEvent?.({
        seq: 1,
        event: { type: 'snapshot_changed', snapshot: routineSnapshot }
      })
    );

    expect(screen.getByRole('heading', { name: 'Make a little movie magic.' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Preparing Review…' })).toBeDisabled();
    expect(
      screen.queryByRole('heading', { name: 'Your films, taking shape.' })
    ).not.toBeInTheDocument();
  });

  it('waits for authoritative monitor capabilities before unlocking the control', async () => {
    const bridge = createMockBridge();
    let receiveEvent: ((event: UiEventEnvelope) => void) | undefined;
    vi.spyOn(bridge, 'openSession').mockImplementation(async (onEvent) => {
      receiveEvent = onEvent;
      return {
        seq: 0,
        snapshot: demoSnapshot(),
        preparing: false,
        submitting: false
      };
    });
    vi.spyOn(bridge, 'pauseJob').mockResolvedValue(undefined);

    render(App, { bridge });
    await waitFor(() => expect(receiveEvent).toBeDefined());
    await fireEvent.click(screen.getByRole('button', { name: /Renders/ }));
    await fireEvent.click(screen.getByRole('button', { name: 'Pause updates' }));
    expect(screen.getByRole('button', { name: 'Updating…' })).toBeDisabled();

    const active = demoSnapshot().jobs[0];
    await act(() =>
      receiveEvent?.({
        seq: 1,
        event: {
          type: 'job_updated',
          job: { ...active, detail: 'An unrelated provider poll arrived first.' }
        }
      })
    );
    expect(screen.getByRole('button', { name: 'Updating…' })).toBeDisabled();

    await act(() =>
      receiveEvent?.({
        seq: 2,
        event: {
          type: 'job_updated',
          job: {
            ...active,
            status: 'paused',
            statusLabel: 'Monitoring paused',
            monitorState: 'paused',
            canResume: true,
            canPause: false
          }
        }
      })
    );
    expect(screen.getByRole('button', { name: 'Resume updates' })).toBeEnabled();
  });

  it('shows a complete copyable provider ID and safe render cleanup choices', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText }
    });
    render(App);
    await fireEvent.click(screen.getByRole('button', { name: /Renders/ }));
    await fireEvent.click(
      screen.getByRole('button', { name: /Macro wildflowers swaying in a soft summer storm/i })
    );

    expect(
      await screen.findByText('fal-request-38c2a011-long-but-fully-visible')
    ).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Copy provider job ID' }));
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith('fal-request-38c2a011-long-but-fully-visible')
    );
    expect(screen.getAllByText('Job identifier copied.').length).toBeGreaterThan(0);

    await fireEvent.click(screen.getByRole('button', { name: /Open in player/ }));
    expect((await screen.findAllByText('Demo mode has no real file to open.')).length).toBeGreaterThan(0);
    expect(screen.queryByText(/Handed off to your system player/)).not.toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Delete render' }));
    expect(screen.getByRole('dialog', { name: 'Remove this render?' })).toBeVisible();
    expect(screen.getByRole('button', { name: 'Remove, keep video' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Delete video too/ })).toBeInTheDocument();
    expect(screen.getByText(/fal\.ai keeps its own copy and job record/i)).toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Remove, keep video' }));
    await waitFor(() =>
      expect(
        screen.queryByRole('button', {
          name: /Macro wildflowers swaying in a soft summer storm/i
        })
      ).not.toBeInTheDocument()
    );
    expect(screen.getAllByText('Render cleared from your reel.').length).toBeGreaterThan(0);
  });

  it('keeps the selected render and detail pane synchronized when filters change', async () => {
    render(App);
    await fireEvent.click(screen.getByRole('button', { name: /Renders/ }));

    await fireEvent.change(screen.getByLabelText('Filter jobs'), {
      target: { value: 'completed' }
    });

    const completed = await screen.findByRole('button', {
      name: /Macro wildflowers swaying in a soft summer storm/i
    });
    await waitFor(() => expect(completed).toHaveAttribute('aria-current', 'true'));
    expect(
      screen.getByRole('heading', {
        name: 'Macro wildflowers swaying in a soft summer storm.'
      })
    ).toBeInTheDocument();
  });

  it('clears unsupported provider-specific controls when switching models', async () => {
    render(App);
    const provider = screen.getByLabelText('Provider');
    await waitFor(() => expect(provider).not.toBeDisabled());

    await fireEvent.change(provider, { target: { value: 'fal' } });

    expect(screen.getByLabelText(/Generated audio/)).toHaveValue('provider_default');
    expect(screen.getByLabelText(/Generated audio/)).toBeDisabled();
    expect(screen.getByLabelText(/Seed/)).toHaveValue('');
    expect(screen.getByLabelText(/Seed/)).toBeDisabled();
    expect(screen.getByText(/Connect fal\.ai backstage/)).toBeInTheDocument();
  });
});
