"use strict";

/* GeoSweep uses LabSweep's map/sidebar interaction, with lazy geographic
   shards because millions of geocaches cannot fit in one MapLibre GeoJSON. */
const $ = id => document.getElementById(id);
const esc = s => String(s ?? "").replace(/[&<>"']/g, c =>
  ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
const names = {2:"Traditional",3:"Multi-cache",4:"Virtual",5:"Letterbox",
  6:"Event",8:"Mystery",11:"Webcam",13:"CITO",137:"EarthCache",
  377:"Wherigo",453:"Mega-event",7005:"Giga-event",4738:"HQ"};
const typeName = id => names[id] || (id == null ? "Unknown type" : "Type " + id);
const number = n => Number(n || 0).toLocaleString();
const ROW = {g:0,n:1,lat:2,lon:3,ty:4,d:5,t:6,size:7,premium:8,
  fav:9,owner:10,region:11,country:12,placed:13};
const MAX_TILES = 20, MAX_MARKERS = 15000, MAX_LIST = 250;
let meta, map, mode = "cluster", sort = "f", selected = null;
let visible = [], shown = [], hitCount = 0, revision = 0;
let searchCatalog = null, searchPromise = null, searchTerm = "";
const cache = new Map();
const byCode = new Map();
const empty = {type:"FeatureCollection",features:[]};
const num = v => v == null || v === "" ? null : +v;
const feature = (lng,lat,props) => ({type:"Feature",
  geometry:{type:"Point",coordinates:[lng,lat]},properties:props});

async function fetchGz(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(url + " (" + response.status + ")");
  if (!response.body || typeof DecompressionStream === "undefined") {
    throw new Error("This viewer requires a browser with gzip stream support.");
  }
  return new Response(response.body.pipeThrough(new DecompressionStream("gzip"))).json();
}

function filters(r,q="") {
  if (q && ![r[0],r[1],r[10]].some(v => String(v ?? "").toLowerCase().includes(q))) return false;
  if ($("ftype").value && +$("ftype").value !== r[4]) return false;
  const minD = num($("fminD").value), minT = num($("fminT").value);
  if (minD !== null && (r[5] == null || r[5] < minD)) return false;
  if (minT !== null && (r[6] == null || r[6] < minT)) return false;
  if ($("fcoords").checked && (r[2] == null || r[3] == null)) return false;
  return true;
}

function sorted(rows) {
  if (sort === "n") return rows.sort((a,b) => (a[1] || a[0]).localeCompare(b[1] || b[0]));
  if (sort === "d") return rows.sort((a,b) => (b[5] ?? 0) - (a[5] ?? 0));
  return rows.sort((a,b) => (b[9] ?? 0) - (a[9] ?? 0));
}

function renderList() {
  const list = $("list");
  const limit = Math.min(MAX_LIST, shown.length);
  $("count").textContent = number(hitCount) + " found";
  if (!limit) {
    list.innerHTML = '<div class="empty">' +
      (searchTerm ? "No matching geocaches." :
        map.getZoom() < 6 ? "Zoom in to browse caches near the map." :
        "No caches in this view. Try moving the map or changing filters.") +
      "</div>";
    return;
  }
  list.replaceChildren();
  const fragment = document.createDocumentFragment();
  for (let i=0;i<limit;i++) {
    const r = shown[i], item = document.createElement("button");
    item.className = "item";
    const name = document.createElement("div");
    name.className = "title";
    name.textContent = r[1] || r[0];
    const details = document.createElement("div");
    details.className = "subtitle";
    details.textContent = r[0] + " · " + typeName(r[4]) +
      (r[5] == null ? "" : " · D " + r[5]) +
      (r[6] == null ? "" : " / T " + r[6]) +
      (r[9] ? " · " + number(r[9]) + " fav" : "") +
      (r[2] == null ? " · coordinates unavailable" : "") ;
    item.append(name, details);
    item.onclick = () => openDetail(r);
    fragment.append(item);
  }
  list.append(fragment);
  if (hitCount > limit) {
    const more = document.createElement("div");
    more.className = "empty";
    more.textContent = "Showing the first " + number(limit) +
      " results. Refine your search or zoom in.";
    list.append(more);
  }
}

function kv(key,value) {
  return value == null || value === "" ? "" :
    '<div><span class="k">' + esc(key) + '</span>' + esc(value) + '</div>';
}
function openDetail(r,fly=true) {
  selected = r;
  $("dtitle").textContent = r[1] || r[0];
  const locationLabel = [r[11],r[12]].filter(Boolean).join(", ");
  $("dbody").innerHTML = '<div class="dsec"><h3>Geocache</h3><div class="kv">' +
    kv("GC code",r[0]) + kv("Type",typeName(r[4])) +
    kv("Difficulty",r[5]) + kv("Terrain",r[6]) +
    kv("Size",r[7]) + kv("Favorite points",r[9]) +
    kv("Owner",r[10]) + kv("Location",locationLabel) +
    kv("Placed",r[13] ? String(r[13]).slice(0,10) : null) +
    kv("Access",r[8] ? "Premium" : "Regular") +
    kv("Latitude",r[2]) + kv("Longitude",r[3]) +
    '</div></div><div class="dsec">' +
    (r[2] == null ? '<p>Coordinates are not available for this premium cache.</p>' : "") +
    '<a target="_blank" rel="noopener noreferrer" href="https://www.geocaching.com/geocache/' +
    encodeURIComponent(r[0]) + '">Open on Geocaching.com ↗</a></div>';
  $("drawer").classList.add("open");
  if (r[2] != null && r[3] != null) {
    map.getSource("selected").setData({type:"FeatureCollection",
      features:[feature(r[3],r[2],{})]});
    if (fly) map.flyTo({center:[r[3],r[2]],zoom:Math.max(9,map.getZoom()),duration:650});
  } else map.getSource("selected").setData(empty);
  const url = new URL(location.href);
  url.searchParams.set("g",r[0]);
  history.replaceState(null,"",url);
}
function closeDetail() {
  selected = null;
  $("drawer").classList.remove("open");
  map.getSource("selected").setData(empty);
  const url = new URL(location.href);
  url.searchParams.delete("g");
  history.replaceState(null,"",url);
}

function tileIds(bounds) {
  const sw = bounds.getSouthWest(), ne = bounds.getNorthEast();
  if (ne.lng - sw.lng > 355) return meta.tiles;
  return meta.tiles.filter(([y,x]) => {
    const south = y*5-90, west=x*5-180;
    const longitudeMatch = west <= ne.lng && west+5 >= sw.lng ||
      west+360 <= ne.lng && west+365 >= sw.lng ||
      west-360 <= ne.lng && west-355 >= sw.lng;
    return south <= ne.lat && south+5 >= sw.lat && longitudeMatch;
  });
}

async function getTile(y,x) {
  const id = y + "-" + x;
  if (!cache.has(id)) {
    const promise = fetchGz("data/tiles/" + id + ".json.gz");
    cache.set(id,promise);
    // Failed requests can be retried when a user returns to the area.
    promise.catch(() => cache.delete(id));
  }
  const result = await cache.get(id);
  // Bound retained memory while allowing recent pan operations to reuse data.
  cache.delete(id);
  cache.set(id, Promise.resolve(result));
  while (cache.size > 22) cache.delete(cache.keys().next().value);
  return result;
}

function updateMap(rows, coarse=false) {
  if (!map || !map.getSource("caches")) return;
  byCode.clear();
  const features = [], flat = [];
  for (const r of rows) {
    if (r[2] == null || r[3] == null) continue;
    if (features.length >= MAX_MARKERS) break;
    byCode.set(r[0],r);
    const f = feature(r[3],r[2],{g:r[0],type:r[4]});
    features.push(f);
    flat.push(f);
  }
  map.getSource("caches").setData({type:"FeatureCollection",features});
  map.getSource("flat").setData({type:"FeatureCollection",features:flat});
  const low = coarse || (map.getZoom() < 6 && !searchTerm);
  const visibleLayer = id => {
    if (id === "density-circles" || id === "density-count") return low && mode !== "heat";
    if (id === "density-heat") return low && mode === "heat";
    if (id === "clusters" || id === "cluster-count" || id === "points")
      return !low && mode === "cluster";
    if (id === "flat-heat") return !low && mode === "heat";
    if (id === "flat-points") return !low && mode === "all";
    return false;
  };
  for (const id of ["density-circles","density-count","density-heat",
    "clusters","cluster-count","points","flat-heat","flat-points"])
    map.setLayoutProperty(id,"visibility",visibleLayer(id)?"visible":"none");
}

async function refreshVisible() {
  if (!meta || !map || !map.getSource("caches")) return;
  const current = ++revision;
  const q = $("search").value.trim().toLowerCase();
  searchTerm = q.length >= 3 ? q : "";
  if (searchTerm) {
    $("hint").textContent = searchCatalog ? "Searching worldwide" : "Loading worldwide search index…";
    if (!searchCatalog) {
      if (!searchPromise) searchPromise = fetchGz("data/catalog.json.gz")
        .then(rows => {searchCatalog=rows;return rows}).catch(err => {searchPromise=null;throw err});
      try { await searchPromise; } catch (e) {
        if (current === revision) $("hint").textContent = "Search unavailable: " + e.message;
        return;
      }
    }
    if (current !== revision) return;
    const matches = [];
    let total = 0;
    for (const r of searchCatalog) {
      if (filters(r,searchTerm)) {
        ++total;
        if (matches.length < 1000) matches.push(r);
      }
    }
    shown = sorted(matches);
    visible = shown;
    hitCount = total;
    $("hint").textContent = "Worldwide search · " + number(total) + " results";
    $("mapnote").textContent = total > MAX_MARKERS ? "Map shows a subset of results" : "";
    updateMap(shown,false);
    renderList();
    return;
  }
  if (q.length) $("hint").textContent = "Enter at least 3 characters for worldwide search";
  else $("hint").textContent = "";
  if (map.getZoom() < 6) {
    visible = [];shown=[];hitCount=0;
    updateMap([],true);
    $("mapnote").textContent = "Zoom in to see individual geocaches";
    renderList();
    return;
  }
  const tiles = tileIds(map.getBounds());
  if (tiles.length > MAX_TILES) {
    visible=[];shown=[];hitCount=0;
    updateMap([],true);
    $("mapnote").textContent = "Zoom in to load individual caches";
    renderList();
    return;
  }
  $("mapnote").textContent = "Loading " + tiles.length + " map tiles…";
  let groups;
  try { groups = await Promise.all(tiles.map(([y,x]) => getTile(y,x))); }
  catch (e) { if (current === revision) $("mapnote").textContent="Map data unavailable: "+e.message; return; }
  if (current !== revision) return;
  const b=map.getBounds(), rows=[];
  for (const group of groups) for (const r of group) {
    if (r[2] == null || r[3] == null) continue;
    if (r[2] < b.getSouth() || r[2] > b.getNorth()) continue;
    const lng=r[3];
    if (!(lng >= b.getWest() && lng <= b.getEast() ||
          lng+360 >= b.getWest() && lng+360 <= b.getEast() ||
          lng-360 >= b.getWest() && lng-360 <= b.getEast())) continue;
    if (filters(r,q)) rows.push(r);
  }
  visible=rows;
  hitCount=rows.length;
  shown=sorted(rows.slice());
  updateMap(shown,false);
  $("mapnote").textContent = rows.length > MAX_MARKERS ?
    "Showing " + number(MAX_MARKERS) + " of " + number(rows.length) +
    " map points · zoom closer for detail" : number(rows.length) + " caches in view";
  renderList();
}

function initMap() {
  map = new maplibregl.Map({
    container:"map",
    style:{version:8,sources:{osm:{type:"raster",tileSize:256,
      tiles:["https://tile.openstreetmap.org/{z}/{x}/{y}.png"],
      attribution:"© OpenStreetMap contributors"}},
      layers:[{id:"osm",type:"raster",source:"osm"}]},
    center:[10,30],zoom:1.6
  });
  map.addControl(new maplibregl.NavigationControl(),"top-right");
  map.on("load",() => {
    const dense = meta.tiles.map(([y,x,count]) =>
      feature(x*5-177.5,y*5-87.5,{count}));
    map.addSource("density",{type:"geojson",data:{type:"FeatureCollection",features:dense}});
    map.addLayer({id:"density-circles",type:"circle",source:"density",
      paint:{"circle-color":"#1c3a5e","circle-radius":
        ["step",["get","count"],5,10,7,100,10,1000,14,10000,20],
        "circle-stroke-color":"#58a6ff","circle-stroke-width":1}});
    map.addLayer({id:"density-count",type:"symbol",source:"density",
      layout:{"text-field":["case",[">=",["get","count"],100],
        ["to-string",["get","count"]],""],"text-size":10},
      paint:{"text-color":"#e6edf3"}});
    map.addLayer({id:"density-heat",type:"heatmap",source:"density",
      layout:{visibility:"none"},paint:{"heatmap-weight":
        ["interpolate",["linear"],["get","count"],0,0,100,0.25,10000,1],
        "heatmap-radius":28,"heatmap-intensity":1.4,"heatmap-opacity":0.85}});
    map.addSource("caches",{type:"geojson",data:empty,cluster:true,clusterRadius:50,clusterMaxZoom:13});
    map.addLayer({id:"clusters",type:"circle",source:"caches",
      filter:["has","point_count"],layout:{visibility:"none"},
      paint:{"circle-color":"#1c3a5e","circle-stroke-color":"#58a6ff",
        "circle-stroke-width":1,"circle-radius":
        ["step",["get","point_count"],12,50,16,1000,24]}});
    map.addLayer({id:"cluster-count",type:"symbol",source:"caches",
      filter:["has","point_count"],
      layout:{"text-field":["get","point_count_abbreviated"],"text-size":11,"visibility":"none"},
      paint:{"text-color":"#e6edf3"}});
    map.addLayer({id:"points",type:"circle",source:"caches",
      filter:["!",["has","point_count"]],layout:{visibility:"none"},
      paint:{"circle-color":"#58a6ff","circle-radius":4,
        "circle-stroke-width":1,"circle-stroke-color":"#0d1117"}});
    map.addSource("flat",{type:"geojson",data:empty});
    map.addLayer({id:"flat-points",type:"circle",source:"flat",layout:{visibility:"none"},
      paint:{"circle-color":"#58a6ff","circle-radius":3,
        "circle-stroke-width":.6,"circle-stroke-color":"#0d1117"}});
    map.addLayer({id:"flat-heat",type:"heatmap",source:"flat",layout:{visibility:"none"},
      paint:{"heatmap-radius":16,"heatmap-intensity":1.3,"heatmap-opacity":.85}});
    map.addSource("selected",{type:"geojson",data:empty});
    map.addLayer({id:"selected-point",type:"circle",source:"selected",
      paint:{"circle-color":"#d29922","circle-radius":9,
        "circle-stroke-color":"#fff","circle-stroke-width":2}});
    map.on("click","density-circles",e => {
      if (!e.features?.length) return;
      map.flyTo({center:e.features[0].geometry.coordinates,zoom:Math.max(6,map.getZoom()+3)});
    });
    map.on("click","clusters",e => {
      const f=e.features?.[0];
      if (!f) return;
      map.getSource("caches").getClusterExpansionZoom(f.properties.cluster_id,(err,z) =>
        !err && map.easeTo({center:f.geometry.coordinates,zoom:z+0.5}));
    });
    for (const layer of ["points","flat-points"]) {
      map.on("click",layer,e => {
        const r=byCode.get(e.features?.[0]?.properties?.g);
        if (r) openDetail(r,false);
      });
    }
    for (const layer of ["density-circles","clusters","points","flat-points"]) {
      map.on("mouseenter",layer,() => map.getCanvas().style.cursor="pointer");
      map.on("mouseleave",layer,() => map.getCanvas().style.cursor="");
    }
    map.on("moveend",refreshVisible);
    refreshVisible();
  });
}

function bindUi() {
  let debounce;
  $("search").addEventListener("input",() => {
    clearTimeout(debounce);
    debounce=setTimeout(refreshVisible,250);
  });
  for (const id of ["ftype","fminD","fminT","fcoords"])
    $(id).addEventListener("change",refreshVisible);
  document.querySelectorAll(".sortbtn").forEach(button =>
    button.onclick=() => {
      sort=button.dataset.sort;
      document.querySelectorAll(".sortbtn").forEach(b=>b.classList.toggle("on",b===button));
      shown=sorted(shown);
      renderList();
    });
  document.querySelectorAll("#mapmode button").forEach(button =>
    button.onclick=() => {
      mode=button.dataset.mode;
      document.querySelectorAll("#mapmode button").forEach(b=>b.classList.toggle("on",b===button));
      updateMap(shown,map.getZoom()<6 || $("mapnote").textContent.startsWith("Zoom in"));
    });
  $("fit").onclick=() => map.flyTo({center:[10,30],zoom:1.6});
  $("random").onclick=() => {
    if (visible.length) openDetail(visible[Math.floor(Math.random()*visible.length)]);
    else $("hint").textContent="Zoom into a region or search to select a cache.";
  };
  $("near").onclick=() => {
    if (!navigator.geolocation) { $("hint").textContent="Geolocation unavailable.";return; }
    navigator.geolocation.getCurrentPosition(p =>
      map.flyTo({center:[p.coords.longitude,p.coords.latitude],zoom:11}),
      () => $("hint").textContent="Location permission unavailable.",
      {timeout:10000});
  };
  $("close").onclick=closeDetail;
  $("share").onclick=async () => {
    if (!selected) return;
    try {
      await navigator.clipboard.writeText(location.href);
      $("share").textContent="copied";
      setTimeout(()=>$("share").textContent="copy link",1000);
    } catch (e) { $("share").textContent="copy failed"; }
  };
  document.addEventListener("keydown",e => {
    if (e.key==="/" && !["INPUT","SELECT","TEXTAREA"].includes(document.activeElement.tagName)) {
      e.preventDefault();$("search").focus();
    } else if (e.key==="Escape") closeDetail();
  });
}
async function boot() {
  try {
    meta=await fetch("data/index.json").then(r => {
      if (!r.ok) throw Error("index.json ("+r.status+")");
      return r.json();
    });
    const coverage=meta.worldwide ? " · "+(100*meta.total/meta.worldwide).toFixed(2)+"% discovered":"";
    $("stats").textContent=number(meta.total)+" caches"+coverage;
    $("fresh").textContent=number(meta.located)+" mapped · "+
      number(meta.coordless)+" without coordinates · updated "+
      new Date(meta.generated).toLocaleString();
    bindUi();
    initMap();
    $("load").style.display="none";
    const g=new URLSearchParams(location.search).get("g");
    if (g) {
      $("search").value=g;
      // Only load the full worldwide index on explicit search / deep link.
      map.once("load",refreshVisible);
      const open=async()=> {
        if (!searchCatalog) {
          if (!searchPromise) searchPromise=fetchGz("data/catalog.json.gz")
            .then(rows=>searchCatalog=rows);
          await searchPromise;
        }
        const r=searchCatalog.find(x=>x[0].toLowerCase()===g.toLowerCase());
        if (r) openDetail(r);
      };
      map.once("load",()=>open().catch(console.error));
    }
  } catch(e) {
    $("load").textContent="Unable to load map data: "+e.message;
    console.error(e);
  }
}
boot();
