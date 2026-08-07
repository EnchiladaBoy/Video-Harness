import './app.css';
import App from './App.svelte';
import { mount } from 'svelte';

const target = document.getElementById('app');

if (!target) {
  throw new Error('Video Harness could not find its application root.');
}

mount(App, { target });
