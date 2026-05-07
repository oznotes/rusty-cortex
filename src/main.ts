import "./app.css";
import { mount } from "svelte";
import App from "./App.svelte";

// Set theme before mount to prevent flash
const theme = localStorage.getItem("theme") || "dark";
document.documentElement.setAttribute("data-theme", theme);

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
