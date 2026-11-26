import "@fontsource-variable/jetbrains-mono";
import { createApp } from "vue";
import App from "./App.vue";
import { installZoom } from "./zoom";

installZoom();

createApp(App).mount("#app");
