import { createMachine, createActor } from 'xstate';

export const uploadMachine = createMachine({
  id:'upload', initial:'accepted',
  states:{
    accepted:{on:{STORE:'queued', SEND:'sending', FAIL:'failed', CANCEL:'refused'}},
    queued:{on:{SEND:'sending', FAIL:'failed', CANCEL:'refused'}},
    sending:{on:{PROGRESS:{}, WAIT:'waiting', COMPLETE:'applied', OFFLINE:'queued', FAIL:'failed', CANCEL:'refused'}},
    waiting:{on:{SEND:'sending', COMPLETE:'applied', OFFLINE:'queued', FAIL:'failed', CANCEL:'refused'}},
    failed:{on:{RETRY:'sending', CANCEL:'refused'}},
    applied:{type:'final'}, refused:{type:'final'},
  },
});

export function uploadActor(receipts, receiptId) {
  const actor = createActor(uploadMachine);
  actor.subscribe(snapshot => receipts.update(receiptId, {phase:snapshot.value}));
  actor.start();
  return actor;
}
