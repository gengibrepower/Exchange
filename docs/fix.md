# FIX 4.4

[← voltar ao README](../README.pt-BR.md)

A porta FIX fala FIX 4.4, com codec e sessão implementados à mão no crate `exchange-fix` (puro: não lê relógio nem toca rede; o instante entra como dado). O `SenderCompID` é o número da conta (a casa, `0`, é recusada) e o `TargetCompID` é `EXCHANGE`; o cliente é o initiator e propõe o `HeartBtInt` no Logon.

- **Sessão**: todo `MsgSeqNum` é conferido. Buraco gera um único `ResendRequest`; o reenvio volta com `PossDupFlag=Y`, e as mensagens administrativas viram `SequenceReset-GapFill`. Duplicata abaixo do esperado com `PossDupFlag` é ignorada; sem ela, a sessão encerra com Logout. Silêncio além do intervalo gera `TestRequest` e, sem resposta, a conexão cai. Seqs e mensagens enviadas sobrevivem à queda da conexão: ao reconectar, o Logon revela o que faltou.
- **Aplicação**: `NewOrderSingle` (`35=D`, `OrdType` 1 mercado, 2 limite, 3 stop e 4 stop-limite, com o gatilho em `StopPx`, tag 99) e `OrderCancelRequest` (`35=F`) entram; saem `ExecutionReport` (`35=8`: New, Trade, Canceled, Expired, Rejected e Triggered, `ExecType=L`, quando o stop dispara), `OrderCancelReject` (`35=9`) e `BusinessMessageReject` (`35=j`). `ClOrdID` já usado é recusado sem chegar ao núcleo, o que protege o cliente que, na dúvida depois de uma queda, manda a mesma ordem de novo.
- **Relatórios sem pedido**: o que acontece com as ordens da sessão por causa do comando de outra (o fill de uma ordem descansando, o disparo de um stop) vira `ExecutionReport` sem o cliente ter mandado nada. Com o cliente desconectado, eles ficam guardados e saem depois do próximo Logon, em ordem: diferente da borda JSON, a sessão FIX não é cortada por acumular, porque o que acumula fora do ar é limitado pelas ordens que a própria conta já tem. Ordens da mesma conta mandadas pelo JSON não aparecem aqui, porque não têm `ClOrdID` nesta sessão.
- **Desvios do padrão**: preço inteiro por lot (sem casas decimais), como na borda JSON; sem `AvgPx`.

A garantia "nem perde nem duplica" é testada de duas formas: um `proptest` com duas sessões num TCP simulado que cai em pontos aleatórios, perde o que estava em trânsito e recebe reenvios a mais (cada lado recebe exatamente o que o outro enviou, uma vez e em ordem); e um teste de integração com o servidor real, em que a conexão cai depois de a ordem chegar ao núcleo e antes de a confirmação ser lida.
