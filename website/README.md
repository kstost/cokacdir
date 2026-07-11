# COKACDIR Website

Marketing and documentation website for [COKACDIR](https://github.com/kstost/cokacdir) - a multi-panel terminal file manager written in Rust.

**Live Site:** [https://cokacdir.cokac.com](https://cokacdir.cokac.com)

## Tech Stack

- **Framework:** React 18.2 + TypeScript 5.3
- **Build Tool:** Vite 8.1
- **Styling:** Tailwind CSS 3.4
- **Animations:** Framer Motion 11.0
- **Icons:** Lucide React

## Project Structure

```
website/
├── src/
│   ├── main.tsx              # Application entry point
│   ├── App.tsx               # Root component
│   ├── index.css             # Global styles & Tailwind imports
│   └── components/
│       ├── Hero.tsx          # Hero section with animated background
│       ├── Features.tsx      # Feature showcase grid
│       ├── Installation.tsx  # Installation instructions
│       ├── Shortcuts.tsx     # Keyboard shortcuts reference
│       ├── TerminalPreview.tsx # Animated terminal mockup
│       ├── Footer.tsx        # Footer with links
│       └── ui/
│           ├── Button.tsx    # Reusable button component
│           ├── Card.tsx      # Card with hover effects
│           └── CodeBlock.tsx # Code block with copy button
├── public/
│   └── ogimg.jpg             # Open Graph image for social sharing
├── index.html                # HTML template
├── package.json              # Dependencies
├── tsconfig.json             # TypeScript configuration
├── tailwind.config.js        # Tailwind customization
├── postcss.config.js         # PostCSS plugins
└── vite.config.ts            # Vite build configuration
```

## Getting Started

### Prerequisites

- Node.js 20.19+ or 22.12+
- npm or yarn

### Installation

```bash
# Navigate to website directory
cd website

# Install dependencies
npm install
```

### Development

```bash
# Start development server
npm run dev
```

The development server will start at `http://localhost:5173`.

### Build

```bash
# Build for production
npm run build
```

Output will be generated in the `dist/` directory.

### Preview

```bash
# Preview production build locally
npm run preview
```

## Components

### Hero

Animated hero section featuring:
- Gradient background with floating orbs
- Project tagline and description
- CTA buttons for installation and GitHub
- Embedded terminal preview

### Features

Six feature cards showcasing:
- **Blazing Fast** - Rust performance, single optimized binary (~15MB)
- **AI-Powered Commands** - Natural language file operations via Claude
- **Multi Panel** - Dynamic multi-panel layout
- **Keyboard Driven** - Full keyboard navigation
- **Viewer & Editor** - Built-in file viewing and editing
- **Process Manager** - System process management

### Installation

Step-by-step installation instructions with:
- One-line bash installer command
- Platform support indicators (macOS, Linux)
- Claude Code integration setup for AI features
- Copy-to-clipboard functionality

### Shortcuts

Keyboard shortcuts reference organized into categories:
- Navigation
- File Operations
- View & Tools
- Selection & AI

### UI Components

- **Button** - Styled button with variants and hover effects
- **Card** - Card container with glassmorphism and glow effects
- **CodeBlock** - Syntax-highlighted code with copy button

## Styling

The website uses a custom dark theme with Tailwind CSS:

```js
// Primary colors
primary: '#3B82F6'     // Blue
cyan: '#22D3EE'
purple: '#A855F7'
green: '#10B981'

// Background
dark: '#0A0A0F'
```

Key design elements:
- Dark mode optimized
- Glassmorphism effects
- Gradient accents
- Smooth scroll-triggered animations

## Deployment

The website is configured for static hosting:

1. Build the production bundle:
   ```bash
   npm run build
   ```

2. Deploy the `dist/` directory to your hosting provider.

The `CNAME` file in the parent directory configures the custom domain `cokacdir.cokac.com` for GitHub Pages.

## Scripts

| Script | Description |
|--------|-------------|
| `npm run dev` | Start development server |
| `npm run build` | Build for production |
| `npm run preview` | Preview production build |

## Related

- [COKACDIR Main Repository](https://github.com/kstost/cokacdir) - The Rust file manager
- [Live Demo](https://cokacdir.cokac.com) - Production website

## License

MIT License - see the main repository [LICENSE](../LICENSE) and
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) for STT/transcriptor and
Whisper-related notices.
