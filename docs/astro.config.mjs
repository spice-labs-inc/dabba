import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// Base path + site come from the deploy environment so the build matches wherever
// Pages actually serves it: root of a private *.pages.github.io URL, or
// spice-labs-inc.github.io/dabba once the repo is public. CI passes DOCS_BASE /
// DOCS_SITE from actions/configure-pages; both fall back to sensible local defaults.
const base = process.env.DOCS_BASE || "/";
const site = process.env.DOCS_SITE || "https://spice-labs-inc.github.io";

export default defineConfig({
  site,
  base,
  integrations: [
    starlight({
      title: "dabba",
      logo: { src: "./src/assets/logo.svg" },
      tagline: "A full Kubernetes platform from one config file.",
      description:
        "dabba brings up a full Kubernetes platform from a single config file — the same way on your laptop and on managed cloud.",
      customCss: ["./src/styles/custom.css"],
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/spice-labs-inc/dabba",
        },
      ],
      editLink: {
        baseUrl: "https://github.com/spice-labs-inc/dabba/edit/main/docs/",
      },
      lastUpdated: true,
      credits: false,
      sidebar: [
        {
          label: "Start here",
          items: [
            { label: "Overview & CLI", link: "/" },
            { label: "Quickstart", link: "/quickstart/" },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Architecture", link: "/architecture/" },
            { label: "Configuration", link: "/configuration/" },
            { label: "Observability", link: "/observability/" },
          ],
        },
      ],
    }),
  ],
});
