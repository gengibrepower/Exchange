//! Cliente de terminal (F8). Fala com o servidor só pelo protocolo da borda
//! ([`exchange_protocol`]); o núcleo nem está no grafo de dependências (decisão 13).

#![forbid(unsafe_code)]

pub mod app;
pub mod book;
pub mod format;
pub mod orders;
pub mod theme;
pub mod ui;
