/**
 * channel/[id].tsx - Channel chat screen
 */
import React, { useEffect, useRef, useState, useCallback } from "react";
import {
  View, Text, FlatList, TextInput, Pressable, StyleSheet,
  SafeAreaView, KeyboardAvoidingView, Platform, Alert,
  ActivityIndicator, Dimensions,
} from "react-native";
import { useLocalSearchParams, useRouter } from "expo-router";
import { store, useStore } from "../lib/store";
import type { Message } from "../lib/api";

const EMOJI_SHORTCUTS = ["👍", "❤️", "😂", "😮", "😢", "🔥"];

function formatTime(iso: string) {
  const d = new Date(iso);
  const now = new Date();
  const diff = now.getTime() - d.getTime();
  if (diff < 86400000) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (diff < 604800000) return d.toLocaleDateString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
  return d.toLocaleDateString();
}

function MessageItem({
  msg,
  onReact,
  onReply,
  onLongPress,
}: {
  msg: Message;
  onReact: (emoji: string) => void;
  onReply: () => void;
  onLongPress: () => void;
}) {
  const [showReactions, setShowReactions] = useState(false);

  return (
    <Pressable
      onLongPress={() => { setShowReactions(!showReactions); onLongPress(); }}
      style={msg.authorId === store.session?.user.id ? styles.myMessage : styles.otherMessage}
    >
      {msg.authorId !== store.session?.user.id && (
        <View style={styles.avatar}>
          <Text style={styles.avatarText}>{msg.authorUsername[0]?.toUpperCase()}</Text>
        </View>
      )}
      <View style={styles.messageContent}>
        {msg.authorId !== store.session?.user.id && (
          <Text style={styles.authorName}>{msg.authorUsername}</Text>
        )}
        {msg.replyTo && (
          <View style={styles.replyIndicator}>
            <Text style={styles.replyText}>Replying to a message</Text>
          </View>
        )}
        {msg.content ? <Text style={styles.messageText}>{msg.content}</Text> : null}
        {msg.embeds?.map((e, i) => (
          <View key={i} style={styles.embed}>
            {e.title ? <Text style={styles.embedTitle}>{e.title}</Text> : null}
            {e.description ? <Text style={styles.embedDesc} numberOfLines={3}>{e.description}</Text> : null}
            {e.url ? <Text style={styles.embedUrl}>{e.url}</Text> : null}
          </View>
        ))}
        {msg.reactions && msg.reactions.length > 0 && (
          <View style={styles.reactions}>
            {msg.reactions.map((r, i) => (
              <Pressable key={i} onPress={() => onReact(r.emoji)} style={[styles.reaction, r.me && styles.reactionMe]}>
                <Text style={styles.reactionEmoji}>{r.emoji}</Text>
                <Text style={[styles.reactionCount, r.me && styles.reactionCountMe]}>{r.count}</Text>
              </Pressable>
            ))}
          </View>
        )}
        <Text style={styles.timestamp}>{formatTime(msg.createdAt)}</Text>
      </View>

      {showReactions && (
        <View style={styles.quickReactions}>
          {EMOJI_SHORTCUTS.map(emoji => (
            <Pressable key={emoji} onPress={() => { onReact(emoji); setShowReactions(false); }} style={styles.quickReaction}>
              <Text style={styles.quickReactionText}>{emoji}</Text>
            </Pressable>
          ))}
        </View>
      )}
    </Pressable>
  );
}

function TypingIndicator({ channelId }: { channelId: string }) {
  const typing = store.typingUsers[channelId];
  if (!typing || typing.length === 0) return null;
  const text = typing.length === 1
    ? typing[0] + " is typing..."
    : typing.slice(0, 2).join(", ") + " are typing...";
  return <Text style={styles.typingIndicator}>{text}</Text>;
}

export default function ChannelScreen() {
  useStore();
  const { id } = useLocalSearchParams<{ id: string }>();
  const router = useRouter();
  const [input, setInput] = useState("");
  const typingTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);
  const [replyTo, setReplyTo] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const flatListRef = useRef<FlatList>(null);

  const channel = store.channels.find(c => c.id === id) ?? store.dmChannels.find(c => c.id === id);
  const messages = (id ? store.messages[id] : []) ?? [];

  useEffect(() => {
    if (id && store.activeChannelId !== id) {
      store.selectChannel(id).catch(console.error);
    }
  }, [id]);

  async function handleSend() {
    if (!input.trim()) return;
    const content = input.trim();
    setInput("");
    setLoading(true);
    try {
      await store.sendMessage(content, replyTo ?? undefined);
      setReplyTo(null);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setLoading(false);
    }
  }

  async function handleReact(messageId: string, emoji: string) {
    const msg = messages.find(m => m.id === messageId);
    if (!msg || !id) return;
    const existing = msg.reactions?.find(r => r.emoji === emoji);
    if (existing?.me) {
      await store.removeReaction(id, messageId, emoji);
    } else {
      await store.addReaction(id, messageId, emoji);
    }
  }

  function handleReply(msg: Message) {
    setReplyTo(msg.id);
    // Could also show preview of replied message
  }

  function handleLoadMore() {
    if (id) store.loadOlderMessages().catch(console.error);
  }

  const renderMessage = useCallback(({ item }: { item: Message }) => (
    <MessageItem
      msg={item}
      onReact={(emoji) => handleReact(item.id, emoji)}
      onReply={() => handleReply(item)}
      onLongPress={() => {}}
    />
  ), [messages]);

  const channelName = channel
    ? ("name" in channel ? channel.name : "DM")
    : "Channel";

  return (
    <SafeAreaView style={styles.container}>
      {/* Header */}
      <View style={styles.header}>
        <Pressable onPress={() => router.back()} style={styles.backBtn}>
          <Text style={styles.backBtnText}>Back</Text>
        </Pressable>
        <View style={styles.headerTitleArea}>
          <Text style={styles.headerTitle}>{channelName}</Text>
          {channel && "topic" in channel && channel.topic ? (
            <Text style={styles.headerTopic} numberOfLines={1}>{channel.topic}</Text>
          ) : null}
        </View>
        {channel && "isE2ee" in channel && channel.isE2ee ? (
          <Text style={styles.e2eeBadge}>E2EE</Text>
        ) : null}
      </View>

      {/* Messages */}
      <FlatList
        ref={flatListRef}
        data={[...messages].reverse()}
        keyExtractor={item => item.id}
        renderItem={renderMessage}
        contentContainerStyle={styles.messageList}
        inverted
        onEndReached={handleLoadMore}
        onEndReachedThreshold={0.3}
        ListEmptyComponent={
          <View style={styles.emptyChannel}>
            <Text style={styles.emptyChannelText}>
              {channelName === "DM" ? "No messages yet" : "This is the beginning of #" + channelName}
            </Text>
          </View>
        }
      />

      <TypingIndicator channelId={id ?? ""} />

      {/* Reply bar */}
      {replyTo && (
        <View style={styles.replyBar}>
          <Text style={styles.replyBarText}>Replying to a message</Text>
          <Pressable onPress={() => setReplyTo(null)}>
            <Text style={styles.replyBarClose}>X</Text>
          </Pressable>
        </View>
      )}

      {/* Composer */}
      <KeyboardAvoidingView
        behavior={Platform.OS === "ios" ? "padding" : "height"}
        keyboardVerticalOffset={90}
      >
        <View style={styles.composer}>
          <View style={styles.inputRow}>
            <TextInput
              style={styles.input}
              placeholder={channelName !== "DM" ? ("Message #" + channelName) : "Message..."}
              placeholderTextColor="#7890b0"
              value={input}
              onChangeText={(text) => {
                setInput(text);
                if (!typingTimerRef.current && id) {
                  store.sendTyping(id).catch(() => {});
                  typingTimerRef.current = setTimeout(() => {
                    typingTimerRef.current = null;
                  }, 4_000);
                }
              }}
              multiline
              maxLength={4000}
              onSubmitEditing={handleSend}
            />
            <Pressable onPress={handleSend} style={styles.sendBtn} disabled={loading || !input.trim()}>
              {loading ? (
                <ActivityIndicator color="#062020" size="small" />
              ) : (
                <Text style={[styles.sendBtnText, !input.trim() && styles.sendBtnDisabled]}>Send</Text>
              )}
            </Pressable>
          </View>
        </View>
      </KeyboardAvoidingView>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: "#030a14" },
  header: { flexDirection: "row", alignItems: "center", padding: 10, backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  backBtn: { paddingRight: 10 },
  backBtnText: { color: "#27c9a5", fontSize: 15, fontWeight: "600" },
  headerTitleArea: { flex: 1 },
  headerTitle: { color: "#d6f4ff", fontSize: 16, fontWeight: "700" },
  headerTopic: { color: "#7890b0", fontSize: 12 },
  e2eeBadge: { backgroundColor: "#27c9a5", color: "#062020", fontSize: 10, fontWeight: "700", paddingHorizontal: 6, paddingVertical: 2, borderRadius: 4, overflow: "hidden" },
  messageList: { paddingHorizontal: 8, paddingVertical: 4 },
  emptyChannel: { flex: 1, justifyContent: "center", alignItems: "center", paddingTop: 100 },
  emptyChannelText: { color: "#7890b0", fontSize: 14 },
  myMessage: { flexDirection: "row", justifyContent: "flex-end", marginBottom: 4 },
  otherMessage: { flexDirection: "row", marginBottom: 4 },
  avatar: { width: 32, height: 32, borderRadius: 16, backgroundColor: "#0b1a30", justifyContent: "center", alignItems: "center", marginRight: 8 },
  avatarText: { color: "#9ec5d5", fontSize: 14, fontWeight: "600" },
  messageContent: { maxWidth: "75%", backgroundColor: "#0b1a30", borderRadius: 12, paddingHorizontal: 12, paddingVertical: 8 },
  authorName: { color: "#9ec5d5", fontSize: 12, fontWeight: "600", marginBottom: 2 },
  replyIndicator: { borderLeftWidth: 2, borderLeftColor: "#27c9a5", paddingLeft: 6, marginBottom: 4 },
  replyText: { color: "#7890b0", fontSize: 11, fontStyle: "italic" },
  messageText: { color: "#d6f4ff", fontSize: 15, lineHeight: 20 },
  embed: { backgroundColor: "#112544", borderRadius: 8, padding: 10, marginTop: 6 },
  embedTitle: { color: "#27c9a5", fontWeight: "600", fontSize: 14, marginBottom: 4 },
  embedDesc: { color: "#9ec5d5", fontSize: 13 },
  embedUrl: { color: "#7890b0", fontSize: 11, marginTop: 4 },
  reactions: { flexDirection: "row", flexWrap: "wrap", marginTop: 6, gap: 4 },
  reaction: { flexDirection: "row", alignItems: "center", backgroundColor: "#112544", borderRadius: 12, paddingHorizontal: 8, paddingVertical: 3 },
  reactionMe: { backgroundColor: "#1e3a5f" },
  reactionEmoji: { fontSize: 14 },
  reactionCount: { color: "#7890b0", fontSize: 12, marginLeft: 4 },
  reactionCountMe: { color: "#27c9a5" },
  timestamp: { color: "#7890b0", fontSize: 10, marginTop: 4, alignSelf: "flex-end" },
  quickReactions: { position: "absolute", bottom: -30, right: 0, flexDirection: "row", backgroundColor: "#0b1a30", borderRadius: 20, padding: 4, gap: 4 },
  quickReaction: { padding: 4 },
  quickReactionText: { fontSize: 18 },
  typingIndicator: { color: "#7890b0", fontSize: 12, fontStyle: "italic", paddingHorizontal: 12, paddingVertical: 2 },
  replyBar: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", backgroundColor: "#0f2040", paddingHorizontal: 12, paddingVertical: 6 },
  replyBarText: { color: "#9ec5d5", fontSize: 13 },
  replyBarClose: { color: "#7890b0", fontSize: 14, fontWeight: "700" },
  composer: { backgroundColor: "#071327", paddingHorizontal: 8, paddingVertical: 8, borderTopWidth: 1, borderTopColor: "#0f2040" },
  inputRow: { flexDirection: "row", alignItems: "flex-end", backgroundColor: "#112544", borderRadius: 20, paddingHorizontal: 14, paddingVertical: 8 },
  input: { flex: 1, color: "#d6f4ff", fontSize: 15, maxHeight: 120 },
  sendBtn: { marginLeft: 8, backgroundColor: "#27c9a5", borderRadius: 14, paddingHorizontal: 14, paddingVertical: 6 },
  sendBtnText: { color: "#062020", fontWeight: "700", fontSize: 14 },
  sendBtnDisabled: { opacity: 0.4 },
});
