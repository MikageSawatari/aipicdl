(function () {
  "use strict";
  var grid = document.getElementById("grid");
  if (!grid) return;
  var cards = Array.prototype.slice.call(grid.querySelectorAll(".card"));
  var tabs = Array.prototype.slice.call(document.querySelectorAll(".tab"));
  var ratings = Array.prototype.slice.call(document.querySelectorAll("input.rating"));
  var q = document.getElementById("q");
  var sort = document.getElementById("sort");
  var album = document.getElementById("album");
  var blur = document.getElementById("blur");
  var count = document.getElementById("count");
  var type = "";
  var KEY = "aipicdl-filter";

  function load() {
    try {
      var s = JSON.parse(localStorage.getItem(KEY) || "{}");
      if (s.ratings) ratings.forEach(function (r) { r.checked = s.ratings.indexOf(r.value) >= 0; });
      if (s.sort) sort.value = s.sort;
      if (typeof s.blur === "boolean") blur.checked = s.blur;
    } catch (e) { /* 保存領域が使えない環境では既定値のまま */ }
  }
  function save() {
    try {
      localStorage.setItem(KEY, JSON.stringify({
        ratings: ratings.filter(function (r) { return r.checked; }).map(function (r) { return r.value; }),
        sort: sort.value,
        blur: blur.checked
      }));
    } catch (e) { /* noop */ }
  }

  function apply() {
    var words = (q.value || "").toLowerCase().split(/\s+/).filter(Boolean);
    var allowed = {};
    ratings.forEach(function (r) { if (r.checked) allowed[r.value] = true; });
    var al = album ? album.value : "";
    var shown = 0;
    cards.forEach(function (c) {
      var ok = (!type || c.dataset.type === type) && allowed[c.dataset.rating];
      if (ok && al) ok = (" " + c.dataset.albums + " ").indexOf(" " + al + " ") >= 0;
      if (ok && words.length) {
        var hay = c.dataset.q;
        for (var i = 0; i < words.length; i++) if (hay.indexOf(words[i]) < 0) { ok = false; break; }
      }
      c.classList.toggle("hidden", !ok);
      if (ok) shown++;
    });
    count.textContent = shown + " / " + cards.length + " 件";
    document.body.classList.toggle("blur-r18", blur.checked);
  }

  function order() {
    var key = sort.value;
    var num = function (c, k) { return Number(c.dataset[k]) || 0; };
    var sorted = cards.slice().sort(function (a, b) {
      if (key === "old") return num(a, "date") - num(b, "date");
      if (key === "likes") return num(b, "likes") - num(a, "likes");
      if (key === "views") return num(b, "views") - num(a, "views");
      return num(b, "date") - num(a, "date");
    });
    var frag = document.createDocumentFragment();
    sorted.forEach(function (c) { frag.appendChild(c); });
    grid.appendChild(frag);
  }

  function fromHash() {
    var m = /[#&]q=([^&]*)/.exec(location.hash);
    if (m) {
      try { q.value = decodeURIComponent(m[1]); } catch (e) { q.value = m[1]; }
    }
  }

  tabs.forEach(function (t) {
    t.addEventListener("click", function () {
      tabs.forEach(function (x) { x.classList.toggle("active", x === t); });
      type = t.dataset.type;
      apply();
    });
  });
  ratings.forEach(function (r) { r.addEventListener("change", function () { save(); apply(); }); });
  q.addEventListener("input", apply);
  sort.addEventListener("change", function () { save(); order(); });
  if (album) album.addEventListener("change", apply);
  blur.addEventListener("change", function () { save(); apply(); });
  window.addEventListener("hashchange", function () { fromHash(); apply(); });

  load();
  fromHash();
  order();
  apply();
})();
