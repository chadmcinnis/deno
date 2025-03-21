// deno-fmt-ignore-file
// deno-lint-ignore-file

// Copyright Joyent and Node contributors. All rights reserved. MIT license.
// Taken from Node 23.9.0
// This file is automatically generated by `tests/node_compat/runner/setup.ts`. Do not modify this file manually.

'use strict';

const common = require('../common');

const { strictEqual, throws, equal } = require('assert');

// Manually converted from https://github.com/web-platform-tests/wpt/blob/master/dom/events/CustomEvent.html
// in order to define the `document` ourselves

{
  const type = 'foo';
  const target = new EventTarget();

  target.addEventListener(type, common.mustCall((evt) => {
    strictEqual(evt.type, type);
  }));

  target.dispatchEvent(new Event(type));
}

{
  throws(() => {
    new Event();
  }, TypeError);
}

{
  const event = new Event('foo');
  equal(event.type, 'foo');
  equal(event.bubbles, false);
  equal(event.cancelable, false);
  equal(event.detail, null);
}
