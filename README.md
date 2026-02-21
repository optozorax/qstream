# QStream MVP

MVP for streamer Q&A:
- `backend`: Rust + Axum API with SQLite storage.
- `frontend`: Svelte single-page app with nickname form + hCaptcha.

## What is implemented

- `POST /api/register`:
  - accepts `{ nickname, hcaptcha_token }`
  - validates hCaptcha on backend (`https://hcaptcha.com/siteverify`)
  - stores/updates user in SQLite table `users`
- `GET /api/health`

## Backend setup

```bash
cd backend
cp .env.example .env
cargo run
```

Default env values:
- `APP_ADDR=0.0.0.0:3000`
- `FRONTEND_ORIGIN=http://localhost:5173`
- `DATABASE_URL=sqlite://qstream.db?mode=rwc`
- `HCAPTCHA_SITE_KEY=...`
- `HCAPTCHA_SECRET=...`
- `HCAPTCHA_SKIP_VERIFY=false`

For local smoke tests only, you can set `HCAPTCHA_SKIP_VERIFY=true`.

## Frontend setup

```bash
cd frontend
cp .env.example .env
npm install
npm run dev
```

Frontend env:
- `VITE_API_BASE_URL=http://localhost:3000`
- `VITE_HCAPTCHA_SITE_KEY=...`

## Local dev flow

1. Start backend on port `3000`.
2. Start frontend on port `5173`.
3. Open `http://localhost:5173`, pass hCaptcha, enter nickname.
4. Check backend DB file: `backend/qstream.db`.
