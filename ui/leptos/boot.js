import init from "/app.js";

init().catch((error) => {
  console.error(error);
  document.body.textContent =
    "The sandbox could not start. Check that WebAssembly is enabled and reload.";
});
