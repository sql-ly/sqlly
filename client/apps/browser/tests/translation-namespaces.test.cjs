'use strict';
// Tests for the /translations namespace filter helper (server/Sqlly.Api/wwwroot/assets/pages/translation-namespaces.js).

const test = require('node:test');
const assert = require('node:assert/strict');
const namespaces = require('../../../../server/Sqlly.Api/wwwroot/assets/pages/translation-namespaces.js');

test('build creates a sorted hierarchy with recursive translation counts', () => {
    const tree = namespaces.build([
        { key: 'settings.appearance.language.title' },
        { key: 'settings.appearance.language.apply' },
        { key: 'settings.general.title' },
        { key: 'about.title' },
    ]);

    assert.deepEqual(tree, [
        {
            key: 'about', name: 'about', count: 1, children: [],
        },
        {
            key: 'settings', name: 'settings', count: 3, children: [
                {
                    key: 'settings.appearance', name: 'appearance', count: 2, children: [
                        { key: 'settings.appearance.language', name: 'language', count: 2, children: [] },
                    ],
                },
                { key: 'settings.general', name: 'general', count: 1, children: [] },
            ],
        },
    ]);
});

test('contains includes the exact namespace and every descendant only on segment boundaries', () => {
    assert.equal(namespaces.contains('settings.appearance.language.title', 'settings'), true);
    assert.equal(namespaces.contains('settings.appearance.language.title', 'settings.appearance'), true);
    assert.equal(namespaces.contains('settings.appearance', 'settings.appearance'), true);
    assert.equal(namespaces.contains('settings.appearance.language.title', 'settings.appear'), false);
    assert.equal(namespaces.contains('settings.general.title', 'settings.appearance'), false);
    assert.equal(namespaces.contains('other.settings.title', 'settings'), false);
});

test('an empty namespace leaves every translation visible', () => {
    assert.equal(namespaces.contains('settings.appearance.language.title', ''), true);
    assert.equal(namespaces.contains('about.title', ''), true);
});
