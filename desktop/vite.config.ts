import { defineConfig } from 'vite';
export default defineConfig({define:{__DESKTOP_SMOKE__:JSON.stringify(process.env.AIBRIDGE_DESKTOP_SMOKE==='1')},server:{port:1420,strictPort:true},clearScreen:false});
