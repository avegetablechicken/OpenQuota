import { mount } from 'svelte';
import App from './App.svelte';
import { desktopPlatform } from './lib/platform';
import './styles/tokens.css';
import './styles/base.css';
import './styles/layout.css';
import './styles/components.css';

document.documentElement.dataset.platform = desktopPlatform();

const target = document.getElementById('app');

if (!target) {
  throw new Error('OpenQuota mount point was not found');
}

mount(App, { target });
