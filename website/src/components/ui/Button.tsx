import { motion } from 'framer-motion'
import { ReactNode } from 'react'

interface ButtonProps {
  children: ReactNode
  variant?: 'primary' | 'secondary'
  href?: string
  onClick?: () => void
  ariaLabel?: string
}

export default function Button({ children, variant = 'primary', href, onClick, ariaLabel }: ButtonProps) {
  const baseClasses = 'inline-flex items-center gap-2 px-6 py-3 rounded-lg font-semibold transition-all duration-300 cursor-pointer'

  const variants = {
    primary: 'bg-gradient-to-r from-primary to-accent-cyan text-white hover:shadow-lg hover:shadow-accent-cyan/25 hover:-translate-y-0.5',
    secondary: 'bg-bg-elevated border border-zinc-700 text-white hover:border-accent-cyan hover:shadow-lg hover:shadow-accent-cyan/10 hover:-translate-y-0.5',
  }

  const props = {
    className: `${baseClasses} ${variants[variant]}`,
    whileHover: { scale: 1.02 },
    whileTap: { scale: 0.98 },
    'aria-label': ariaLabel,
  }

  if (href) {
    return (
      <motion.a
        href={href}
        target={href.startsWith('http') ? '_blank' : undefined}
        rel={href.startsWith('http') ? 'noopener noreferrer' : undefined}
        {...props}
      >
        {children}
      </motion.a>
    )
  }

  return (
    <motion.button onClick={onClick} {...props}>
      {children}
    </motion.button>
  )
}
