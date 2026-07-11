import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  base: './',
  build: {
    outDir: 'dist',
    assetsDir: 'assets',
    // Content-hashed assets referenced by a recently cached index must remain
    // available across deployments. buildweb.sh publishes the new index last
    // and likewise keeps older root assets instead of creating a 404 window.
    emptyOutDir: false,
  }
})
