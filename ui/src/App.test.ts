import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import App from './App.svelte';

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
});
