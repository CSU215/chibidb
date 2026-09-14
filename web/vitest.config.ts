import { defineConfig } from 'vitest/config'

export default defineConfig({
  // The node environment is enough: the tests stub `fetch` and `sessionStorage`
  // themselves, so jsdom would be weight for nothing. What is worth testing here
  // is the contract with the engine -- request shape, error mapping -- not
  // component rendering.
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
})
