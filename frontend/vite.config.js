import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

const allowedHosts = [
  ...((process.env.VITE_ALLOWED_HOSTS ?? '')
    .split(',')
    .map((host) => host.trim())
    .filter(Boolean)),
]

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
  server: {
    host: '0.0.0.0',
    allowedHosts,
  },
})
