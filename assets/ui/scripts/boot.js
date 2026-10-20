let notificationVersion=null,refreshTimer=null;
window.stateChanged=function(version){if(JSON.stringify(notificationVersion)===JSON.stringify(version))return;notificationVersion=version;if(refreshInFlight){refreshQueued=true;return;}if(!refreshTimer)refreshTimer=setTimeout(()=>{refreshTimer=null;void refreshState(false);},80);};
document.addEventListener('visibilitychange',()=>{if(!document.hidden)void refreshState(true);});
refreshState(true);setInterval(()=>{if(state?.ai_busy)status()},1000);
