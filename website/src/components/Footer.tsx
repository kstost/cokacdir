import { Github, FileText, Cpu } from 'lucide-react'

export default function Footer() {
  return (
    <footer className="py-12 px-4 border-t border-zinc-800">
      <div className="max-w-6xl mx-auto">
        <div className="flex flex-col md:flex-row items-center justify-between gap-6">
          {/* Logo & tagline */}
          <div className="text-center md:text-left">
            <h3 className="text-xl font-bold gradient-text mb-1">cokacdir</h3>
            <p className="text-zinc-500 text-sm">The blazing fast terminal file manager</p>
          </div>

          {/* Links */}
          <div className="flex items-center gap-6">
            <a
              href="https://github.com/kstost/cokacdir"
              target="_blank"
              rel="noopener noreferrer"
              className="flex items-center gap-2 text-zinc-400 hover:text-white transition-colors"
            >
              <Github className="w-5 h-5" />
              <span className="text-sm">GitHub</span>
            </a>
            <span className="flex items-center gap-2 text-zinc-500">
              <Cpu className="w-5 h-5" />
              <span className="text-sm">Built with Rust</span>
            </span>
            <a
              href="https://github.com/kstost/cokacdir/blob/main/LICENSE"
              target="_blank"
              rel="noopener noreferrer"
              className="flex items-center gap-2 text-zinc-400 hover:text-white transition-colors"
            >
              <FileText className="w-5 h-5" />
              <span className="text-sm">MIT License</span>
            </a>
          </div>
        </div>

        {/* Copyright */}
        <div className="mt-8 pt-6 border-t border-zinc-800 text-center">
          <p className="text-zinc-500 text-sm">
            © 2026 <a href="https://cokacdir.cokac.com" className="text-accent-cyan hover:underline">cokac</a>. All rights reserved.
          </p>
        </div>
      </div>
    </footer>
  )
}
